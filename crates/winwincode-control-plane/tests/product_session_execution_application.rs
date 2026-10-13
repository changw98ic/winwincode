// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, from_value};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    AppendAssistantMessageCommand, AssistantMessageState, CancelProductSessionCommand,
    ControlPlane, ControlPlaneConfig, CreateProductSessionCommand, CredentialReferenceService,
    DurableExecutionPortContext, DurableExecutionPortDelegate, DurableExecutionPortError,
    DurableExecutionPortIngress, DurableExecutionPortSupplement,
    DurableProviderGatewayIdentitySource, EventPublishError, EventPublisher,
    LocalModelPolicyAuthority, LocalModelPolicyAuthorityConfig, LocalSecretStoreAdapter,
    ModelAdmissionClock, ModelAdmissionClockError, ModelAdmissionLimits, ModelAdmissionPolicyLayer,
    ModelCapability, ModelExecutionOpenReceipt, ModelExecutionPortReceipt, ModelRequestPoolConfig,
    ModelRoutePolicyDecision, ModelSettingsRequest, ModelSettingsService, ModelSettingsTarget,
    ModelSettingsValues, ModelToolSupport, OutboxEvent, ProductSessionCommandContext,
    ProductSessionExecutionApplication, ProductSessionExecutionConfig, ProductSessionService,
    ProviderAdmissionReservationConfig, ProviderCatalogRequest, ProviderCatalogService,
    ProviderDescriptor, ProviderGatewayIdentityPort, RepositoryExecutionScheduler, ResolvedSecret,
    StandaloneModelExecutionApplication, StandaloneModelExecutionConfig,
    StandaloneModelExecutionError, StandaloneProviderConfig, StructuredOutputSupport,
    SubmitChatMessageCommand, local_loopback_retry_policy,
};
use winwincode_domain::{
    CodexThreadId, ControlPlaneEventId, CredentialReferenceId, ExecutionEventId, ExecutionJobId,
    ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId, ProductSessionId,
    RequestId, Revision, SchemaVersion, SessionBindingSourceIdentity,
    SessionBindingSourceIdentityKind, SessionIdentity, Sha256Digest, UserId, WorkerId,
    WorkerInstanceId, WorkerSessionId,
};
use winwincode_domain::{
    OrganizationId, ProjectId, RepositoryId, RepositoryScope, RepositoryScopeKind, UserActor,
    UserActorKind, WorkspaceId,
};
use winwincode_execution_port::generated::{
    ExecutionEventCategory, ExecutionEventRecord, ExecutionJob, ExecutionLeaseStamp,
    ExecutionOutcomeStatus, ExecutionPortErrorCode, ExecutionPortMessage, JobCancelAckMessage,
    JobCancelAckMessageStatus, JobCancelMessage, JobCancelMessageKind, JobCancelMessageReason,
    JobDispatchResultMessage, JobDispatchResultMessageKind, JobDispatchResultMessageStatus,
    JobOutcomeAckMessageStatus, JobOutcomeMessage, LeaseWriteStatus, ModelOpenMessage,
    RuntimeEventMessage, RuntimeEventMessageKind, SessionBindingMessage,
};
use winwincode_execution_port::transport::{FrameDirection, TypedFrame};
use winwincode_storage::{
    AuthenticatedWorkerPlacement, EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary,
    ExecutionAdmissionLimits, ExecutionAdmissionPolicy, ExecutionJobState,
    ExecutionJobTransitionRequest, ExecutionLeaseClaim, ExecutionLeaseTerminalOutcome,
    ExecutionLeaseTerminalRequest, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, ExecutionReservationState,
    ProductStateStorage, PublicEventActor, PublicEventScope, ReceiptScopeKey,
    RepositorySchedulerClaimRequest, RepositorySchedulerRetryRequest, RepositorySchedulerScope,
    RepositorySchedulerTerminalRequest, SchedulerRetryPolicy, SqliteStorage,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest, WorkerRegistryScope, WorkerSlotAuthority, WorkerSlotOpenRequest,
    WorkerSlotResourceLimits, WorkerSlotResources, WorkerSlotState, public_receipt_identity,
    receipt_scope_key,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2032-01-01T00:00:{:02}.000Z", second % 60))
}

fn expired() -> Instant {
    Instant("2032-01-01T00:01:00.000Z".to_owned())
}

fn public_repository_scope(scope: &RepositoryScope) -> PublicEventScope {
    PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    }
}

fn execution_message(kind: &str) -> ExecutionPortMessage {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("ExecutionPort fixture");
    fixture["messages"]
        .as_array()
        .expect("fixture messages")
        .iter()
        .find(|message| message["kind"] == kind)
        .cloned()
        .map_or_else(|| panic!("{kind} fixture"), from_value)
        .unwrap_or_else(|error| panic!("{kind} decode: {error}"))
}

struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

struct NoopDelegate;

impl DurableExecutionPortDelegate for NoopDelegate {
    fn accept(
        &mut self,
        _context: DurableExecutionPortContext<'_>,
        _supplement: DurableExecutionPortSupplement<'_>,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        Ok(Vec::new())
    }
}

struct Fixture {
    root: PathBuf,
    secret_root: PathBuf,
    control_plane: ControlPlane,
    storage: SqliteStorage,
    repository_scope: RepositoryScope,
    receipt_scope: ReceiptScopeKey,
    product_session_id: ProductSessionId,
}

impl Fixture {
    fn open(seed: u64) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-product-session-terminal-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("test directory");
        let secret_root = root.with_extension("secrets");
        let _ = fs::remove_dir_all(&secret_root);
        fs::create_dir_all(&secret_root).expect("SecretStore directory");
        let control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane");
        let storage = SqliteStorage::open(&root).expect("storage");
        let repository_scope = RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId(id("org", seed)),
            workspace_id: WorkspaceId(id("wsp", seed)),
            project_id: ProjectId(id("prj", seed)),
            repository_id: RepositoryId(id("rep", seed)),
        };
        let receipt_scope =
            receipt_scope_key(&public_repository_scope(&repository_scope)).expect("receipt scope");
        Self {
            root,
            secret_root,
            control_plane,
            storage,
            repository_scope,
            receipt_scope,
            product_session_id: ProductSessionId(id("psn", seed)),
        }
    }

    fn restart(self) -> Self {
        let Self {
            root,
            secret_root,
            control_plane,
            storage,
            repository_scope,
            receipt_scope,
            product_session_id,
        } = self;
        Box::new(storage).close().expect("storage restart close");
        control_plane
            .shutdown()
            .expect("Control Plane restart close");
        let control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane restart");
        let storage = SqliteStorage::open(&root).expect("storage restart");
        Self {
            root,
            secret_root,
            control_plane,
            storage,
            repository_scope,
            receipt_scope,
            product_session_id,
        }
    }

    fn accept(
        &mut self,
        message: &ExecutionPortMessage,
        server_time: Instant,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        let mut delegate = ProductSessionExecutionApplication::new(NoopDelegate);
        DurableExecutionPortIngress::with_delegate(
            &mut self.control_plane,
            &mut self.storage,
            &self.repository_scope,
            server_time,
            &mut delegate,
        )?
        .handle(message)
    }

    fn record(&mut self) -> winwincode_control_plane::ProductSessionRecord {
        ProductSessionService::new(&mut self.storage)
            .get(&self.receipt_scope, &self.product_session_id)
            .unwrap()
            .unwrap()
    }

    fn close(self) {
        Box::new(self.storage).close().expect("storage close");
        self.control_plane.shutdown().expect("Control Plane close");
        fs::remove_dir_all(self.root).expect("test directory release");
        fs::remove_dir_all(self.secret_root).expect("SecretStore directory release");
    }
}

fn command_context(
    scope: &RepositoryScope,
    request_seed: u64,
    expected_revision: u64,
) -> ProductSessionCommandContext {
    let actor = PublicEventActor::User {
        id: UserId(id("usr", 1)),
    };
    let public_scope = public_repository_scope(scope);
    ProductSessionCommandContext {
        receipt_identity: public_receipt_identity(
            &actor,
            &public_scope,
            RequestId(id("req", request_seed)),
        )
        .expect("receipt identity"),
        expected_revision,
        event_id: ControlPlaneEventId(id("evt", request_seed)),
        occurred_at: at(expected_revision),
        public_actor: actor,
        public_scope,
    }
}

fn create_chat_job(fixture: &mut Fixture, seed: u64) -> ExecutionJob {
    let mut service = ProductSessionService::new(&mut fixture.storage);
    service
        .create(&CreateProductSessionCommand {
            context: command_context(&fixture.repository_scope, seed * 100, 0),
            product_session_id: fixture.product_session_id.clone(),
            project_id: fixture.repository_scope.project_id.clone(),
            repository_id: fixture.repository_scope.repository_id.clone(),
            title: "Terminal replay".to_owned(),
            model_route: ModelRoute {
                credential_reference_id: CredentialReferenceId(id("crd", seed)),
                model_id: "fixture-model".to_owned(),
                provider_id: "fixture-provider".to_owned(),
            },
        })
        .expect("create ProductSession");
    let execution_config = ProductSessionExecutionConfig::try_new(
        fixture.repository_scope.clone(),
        "0123456789abcdef0123456789abcdef01234567",
        "codex-chat",
        Some(3_600),
        1_073_741_824,
    )
    .expect("execution config");
    let receipt = service
        .submit_chat(&SubmitChatMessageCommand {
            attachments: Vec::new(),
            context: command_context(&fixture.repository_scope, seed * 100 + 1, 1),
            product_session_id: fixture.product_session_id.clone(),
            message: "run the canonical Chat turn".to_owned(),
            execution_config,
        })
        .expect("submit Chat");
    let scope = execution_scope(fixture);
    let record = fixture
        .storage
        .execution_queue()
        .expect("queue")
        .load_job(&scope, &receipt.turn_intent.execution_job_id)
        .expect("load execution Job")
        .expect("execution Job");
    serde_json::from_slice(&record.dispatch_payload).expect("ExecutionJob JSON")
}

fn execution_scope(fixture: &Fixture) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: fixture.repository_scope.organization_id.clone(),
        workspace_id: fixture.repository_scope.workspace_id.clone(),
        project_id: fixture.repository_scope.project_id.clone(),
        repository_id: fixture.repository_scope.repository_id.clone(),
        product_session_id: fixture.product_session_id.clone(),
        delivery_id: None,
    }
}

fn worker_pool(seed: u64) -> WorkerPoolId {
    WorkerPoolId(id("wpl", seed))
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", 1)),
        kind: UserActorKind::User,
    })
}

fn organization_scope(fixture: &Fixture) -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: fixture.repository_scope.organization_id.clone(),
    }
}

fn configure_model_provider(fixture: &mut Fixture, seed: u64) {
    let provider_id = "fixture-provider";
    let model_id = "fixture-model";
    let credential_reference_id = CredentialReferenceId(id("crd", seed));
    let organization_scope = organization_scope(fixture);
    ProviderCatalogService::new(&mut fixture.storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: Scope::OrganizationScope(organization_scope.clone()),
                request_id: RequestId(id("req", seed * 100 + 40)),
                expected_catalog_version: 0,
            },
            &ProviderDescriptor {
                provider_id: provider_id.to_owned(),
                display_name: "Fixture Provider".to_owned(),
                adapter_kind: "deterministic-loopback".to_owned(),
                credential_reference_id: credential_reference_id.clone(),
                models: vec![ModelCapability {
                    model_id: model_id.to_owned(),
                    display_name: "Fixture Model".to_owned(),
                    context_window_tokens: 128_000,
                    max_output_tokens: 16_000,
                    tool_support: ModelToolSupport::Parallel,
                    structured_output_support: StructuredOutputSupport::JsonSchemaStrict,
                    reasoning_efforts: vec!["high".to_owned()],
                }],
            },
        )
        .expect("register fixture Provider");
    CredentialReferenceService::new(&mut fixture.storage)
        .create(
            &CredentialReferenceCreateCommand {
                actor: actor(),
                command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
                expected_revision: Revision(0),
                payload: CredentialReferenceCreatePayload {
                    credential_reference_id: credential_reference_id.clone(),
                    display_name: "Fixture Credential".to_owned(),
                    provider_id: provider_id.to_owned(),
                    vault_locator: "local-production://product-session-fixture".to_owned(),
                },
                request_id: RequestId(id("req", seed * 100 + 41)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::OrganizationScope(organization_scope.clone()),
            },
            1_956_528_000_000,
        )
        .expect("create fixture Credential reference");
    ModelSettingsService::new(&mut fixture.storage)
        .update(
            &ModelSettingsRequest {
                actor: actor(),
                target: ModelSettingsTarget::ProductSession {
                    repository_scope: fixture.repository_scope.clone(),
                    product_session_id: fixture.product_session_id.clone(),
                },
                request_id: RequestId(id("req", seed * 100 + 42)),
                expected_revision: 0,
            },
            ModelSettingsValues {
                default_model_route: Some(ModelRoute {
                    credential_reference_id: credential_reference_id.clone(),
                    model_id: model_id.to_owned(),
                    provider_id: provider_id.to_owned(),
                }),
                worker_concurrency_limit: 1,
            },
            Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("configure ProductSession model route");
    let resolution = CredentialReferenceService::new(&mut fixture.storage)
        .resolve(
            &Scope::OrganizationScope(organization_scope),
            &credential_reference_id,
        )
        .expect("resolve fixture Credential reference");
    LocalSecretStoreAdapter::open(&fixture.secret_root)
        .expect("open fixture SecretStore")
        .store(
            &resolution,
            ResolvedSecret::from_bytes(b"product-session-provider-secret".to_vec())
                .expect("fixture secret"),
        )
        .expect("store fixture secret");
}

struct FixedClock;

impl ModelAdmissionClock for FixedClock {
    fn unix_minute(&self) -> Result<u64, ModelAdmissionClockError> {
        Ok(32_608_800)
    }
}

fn model_policy() -> LocalModelPolicyAuthority {
    let base = ModelAdmissionPolicyLayer::try_new(
        "product-session-provider-policy".to_owned(),
        1,
        "budget-2032-01".to_owned(),
        ModelRoutePolicyDecision::Allow,
        ModelAdmissionLimits {
            requests_per_minute: 100,
            tokens_per_minute: 100_000,
            concurrent_requests: 100,
            token_budget: 1_000_000,
            cost_budget_micros: Some(1_000_000),
        },
    )
    .expect("model admission policy");
    LocalModelPolicyAuthority::try_new(LocalModelPolicyAuthorityConfig { base })
        .expect("model policy authority")
}

fn model_pool_config() -> ModelRequestPoolConfig {
    ModelRequestPoolConfig {
        max_routes: 4,
        max_active_per_route: 1,
        max_waiting_per_route: 4,
        max_exchange_records_per_route: 8,
        max_buffered_frames_per_stream: 32,
        max_buffered_bytes_per_stream: 64 * 1024,
        resume_buffered_frames_per_stream: 8,
        resume_buffered_bytes_per_stream: 16 * 1024,
    }
}

fn model_application(fixture: &Fixture) -> StandaloneModelExecutionApplication {
    try_model_application(fixture).expect("standalone model application")
}

fn try_model_application(
    fixture: &Fixture,
) -> Result<StandaloneModelExecutionApplication, StandaloneModelExecutionError> {
    StandaloneModelExecutionApplication::open_with_clock(
        StandaloneModelExecutionConfig {
            data_directory: fixture.root.clone(),
            secret_directory: fixture.secret_root.clone(),
            providers: vec![StandaloneProviderConfig::Loopback {
                provider_id: "fixture-provider".to_owned(),
            }],
            admission: ProviderAdmissionReservationConfig::try_new(100, 10)
                .expect("Provider reservation config"),
            pool: model_pool_config(),
            policy: Box::new(model_policy()),
            retry_policy: Box::new(local_loopback_retry_policy().expect("loopback retry policy")),
        },
        Box::new(FixedClock),
    )
}

fn provider_open(
    application: &mut StandaloneModelExecutionApplication,
    message: &ModelOpenMessage,
) -> winwincode_control_plane::ProviderGatewayOpenReceipt {
    let frame = TypedFrame::new(
        FrameDirection::WorkerToControlPlane,
        ExecutionPortMessage::ModelOpenMessage(message.clone()),
    )
    .expect("typed ModelOpen");
    let ModelExecutionPortReceipt::Opened(ModelExecutionOpenReceipt::Opened { gateway, .. }) =
        application
            .accept_local(&frame)
            .expect("Provider ModelOpen")
    else {
        panic!("Provider request must open immediately");
    };
    gateway
}

fn assistant_content(fixture: &mut Fixture) -> String {
    ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .expect("ProductSession read")
        .expect("ProductSession")
        .messages()
        .iter()
        .find(|message| message.role == "assistant")
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

fn register_worker(storage: &mut SqliteStorage, seed: u64) {
    register_worker_process(storage, seed, seed, seed * 100 + 2, 1);
}

fn register_worker_process(
    storage: &mut SqliteStorage,
    worker_seed: u64,
    instance_seed: u64,
    request_seed: u64,
    started_second: u64,
) {
    let registration = WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "product-session-terminal-test".to_owned(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".to_owned()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: "local".to_owned(),
        max_slots: 1,
        message_id: ExecutionMessageId(id("xmsg", request_seed)),
        request_id: RequestId(id("req", request_seed)),
        sent_at: at(started_second),
        started_at: at(started_second),
        worker_id: WorkerId(id("wrk", worker_seed)),
        worker_instance_id: WorkerInstanceId(id("wki", instance_seed)),
    };
    let heartbeat = WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 1,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots: 1,
        running_slots: 0,
        message_id: ExecutionMessageId(id("xmsg", request_seed + 1)),
        observed_at: at(started_second + 1),
        sent_at: at(started_second + 1),
        worker_id: registration.worker_id.clone(),
        worker_instance_id: registration.worker_instance_id.clone(),
    };
    let mut registry = storage.execution_registry().expect("registry");
    registry
        .register_worker(&registration)
        .expect("register Worker");
    registry
        .record_heartbeat(&heartbeat)
        .expect("heartbeat Worker");
}

fn admission_boundaries(
    scope: &ExecutionQueueScope,
    pool: &WorkerPoolId,
) -> Vec<ExecutionAdmissionBoundary> {
    vec![
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
            worker_pool_id: pool.clone(),
        },
    ]
}

fn reserve_execution(
    storage: &mut SqliteStorage,
    scope: &ExecutionQueueScope,
    job: &ExecutionJob,
    seed: u64,
) {
    let pool = worker_pool(seed);
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 2,
        max_queued: 2,
        token_budget: Some(100_000),
        cost_budget_microunits: Some(1_000_000),
        max_runtime_millis: Some(60_000),
    };
    let mut admission = storage.execution_admission().expect("admission");
    for boundary in admission_boundaries(scope, &pool) {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("admission policy");
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: scope.clone(),
            user_id: UserId(id("usr", 1)),
            worker_pool_id: pool.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", seed * 100 + 4)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: Some(100),
            reserved_cost_microunits: Some(1_000),
            runtime_limit_millis: Some(30_000),
            submitted_at: at(4),
        })
        .expect("reserve execution");
    admission
        .start(&ExecutionReservationStart {
            scope: scope.clone(),
            worker_pool_id: pool,
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", seed * 100 + 5)),
            expected_revision: 1,
            started_at: at(5),
        })
        .expect("start execution");
}

struct RuntimeAuthority {
    lease: ExecutionLeaseStamp,
    slot: WorkerSlotAuthority,
}

fn scheduler_scope(fixture: &Fixture) -> RepositorySchedulerScope {
    RepositorySchedulerScope {
        organization_id: fixture.repository_scope.organization_id.clone(),
        workspace_id: fixture.repository_scope.workspace_id.clone(),
        project_id: fixture.repository_scope.project_id.clone(),
        repository_id: fixture.repository_scope.repository_id.clone(),
    }
}

fn scheduler_claim(
    fixture: &mut Fixture,
    worker_seed: u64,
    instance_seed: u64,
    request_seed: u64,
    generation: &str,
    issued_second: u64,
) -> winwincode_execution_port::generated::JobDispatchMessage {
    let scope = scheduler_scope(fixture);
    RepositoryExecutionScheduler::new(&mut fixture.storage)
        .claim_next(&RepositorySchedulerClaimRequest {
            scope,
            request_id: RequestId(id("req", request_seed)),
            scheduler_generation: generation.to_owned(),
            worker_id: WorkerId(id("wrk", worker_seed)),
            worker_instance_id: WorkerInstanceId(id("wki", instance_seed)),
            issued_at: at(issued_second),
            expires_at: at(issued_second + 20),
        })
        .expect("scheduler claim")
        .expect("scheduler dispatch")
}

fn accept_scheduler_dispatch(
    fixture: &mut Fixture,
    dispatch: &winwincode_execution_port::generated::JobDispatchMessage,
    worker_session_id: &WorkerSessionId,
    request_seed: u64,
    second: u64,
) {
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(JobDispatchResultMessage {
                error: None,
                job_id: dispatch.job.job_id.clone(),
                kind: JobDispatchResultMessageKind::JobDispatchResult,
                lease: dispatch.lease.clone(),
                message_id: ExecutionMessageId(id("xmsg", request_seed)),
                payload_digest: dispatch.job.payload_digest.clone(),
                request_id: dispatch.request_id.clone(),
                schema_version: SchemaVersion::WinwincodeV1,
                sent_at: at(second),
                status: JobDispatchResultMessageStatus::Accepted,
                worker_session_id: Some(worker_session_id.clone()),
            }),
            at(second),
        )
        .expect("scheduler dispatch result");
}

fn open_scheduler_runtime(
    fixture: &mut Fixture,
    dispatch: &winwincode_execution_port::generated::JobDispatchMessage,
    session_seed: u64,
    thread_seed: u64,
    request_seed: u64,
    second: u64,
) -> RuntimeAuthority {
    let slot = WorkerSlotAuthority {
        worker_id: dispatch.lease.worker_id.clone(),
        worker_instance_id: dispatch.lease.worker_instance_id.clone(),
        worker_session_id: WorkerSessionId(id("wsn", session_seed)),
        codex_thread_id: CodexThreadId(id("cdx", thread_seed)),
        job_id: dispatch.job.job_id.clone(),
        lease_id: dispatch.lease.lease_id.clone(),
        attempt: u64::try_from(dispatch.lease.attempt).expect("positive dispatch attempt"),
        fencing_token: dispatch.lease.fencing_token.clone(),
    };
    let mut slots = fixture.storage.worker_session_slots().expect("slots");
    slots
        .configure_resources(
            &slot.worker_id,
            &slot.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000,
                max_disk_bytes: 1_000,
                max_processes: 4,
            },
        )
        .expect("slot resources");
    slots
        .open(&WorkerSlotOpenRequest {
            authority: slot.clone(),
            resources: WorkerSlotResources {
                memory_bytes: 10,
                disk_bytes: 10,
                process_slots: 1,
            },
            request_id: RequestId(id("req", request_seed)),
            opened_at: at(second),
        })
        .expect("open scheduler slot");
    RuntimeAuthority {
        lease: dispatch.lease.clone(),
        slot,
    }
}

fn claim_and_open_runtime(
    fixture: &mut Fixture,
    job: &ExecutionJob,
    seed: u64,
) -> RuntimeAuthority {
    let claim = ExecutionLeaseClaim {
        expires_at: at(50),
        fencing_token: FencingToken(seed.to_string()),
        issued_at: at(5),
        job_id: job.job_id.clone(),
        lease_id: LeaseId(id("lse", seed)),
        message_id: ExecutionMessageId(id("xmsg", seed * 100 + 6)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(id("req", seed * 100 + 6)),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", seed)),
        attempt: 1,
    };
    fixture
        .storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job(&claim)
        .expect("claim execution");
    let slot = WorkerSlotAuthority {
        worker_id: claim.worker_id.clone(),
        worker_instance_id: claim.worker_instance_id.clone(),
        worker_session_id: WorkerSessionId(id("wsn", seed)),
        codex_thread_id: CodexThreadId(id("cdx", seed)),
        job_id: job.job_id.clone(),
        lease_id: claim.lease_id.clone(),
        attempt: 1,
        fencing_token: claim.fencing_token.clone(),
    };
    let mut slots = fixture.storage.worker_session_slots().expect("slots");
    slots
        .configure_resources(
            &slot.worker_id,
            &slot.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000,
                max_disk_bytes: 1_000,
                max_processes: 4,
            },
        )
        .expect("slot resources");
    slots
        .open(&WorkerSlotOpenRequest {
            authority: slot.clone(),
            resources: WorkerSlotResources {
                memory_bytes: 10,
                disk_bytes: 10,
                process_slots: 1,
            },
            request_id: RequestId(id("req", seed * 100 + 7)),
            opened_at: at(6),
        })
        .expect("open slot");
    RuntimeAuthority {
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: claim.expires_at,
            fencing_token: claim.fencing_token,
            issued_at: claim.issued_at,
            job_id: claim.job_id,
            lease_id: claim.lease_id,
            worker_id: claim.worker_id,
            worker_instance_id: claim.worker_instance_id,
        },
        slot,
    }
}

fn transition_job(
    storage: &mut SqliteStorage,
    scope: &ExecutionQueueScope,
    job_id: &ExecutionJobId,
    seed: u64,
    expected_revision: u64,
    from: ExecutionJobState,
    to: ExecutionJobState,
) {
    storage
        .execution_queue()
        .expect("queue")
        .transition(&ExecutionJobTransitionRequest {
            scope: scope.clone(),
            job_id: job_id.clone(),
            request_id: RequestId(id("req", seed)),
            expected_revision,
            from,
            to,
            occurred_at: at(6 + expected_revision),
        })
        .expect("execution Job transition");
}

fn accept_dispatch(
    fixture: &mut Fixture,
    job: &ExecutionJob,
    runtime: &RuntimeAuthority,
    seed: u64,
) {
    let message = JobDispatchResultMessage {
        error: None,
        job_id: job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: runtime.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", seed * 100 + 8)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(id("req", seed * 100 + 6)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at(8),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(runtime.slot.worker_session_id.clone()),
    };
    DurableExecutionPortIngress::new(
        &mut fixture.control_plane,
        &mut fixture.storage,
        &fixture.repository_scope,
        at(8),
    )
    .expect("base ingress")
    .handle(&ExecutionPortMessage::JobDispatchResultMessage(message))
    .expect("accepted dispatch");
}

fn session_identity(fixture: &Fixture, runtime: &RuntimeAuthority) -> SessionIdentity {
    SessionIdentity {
        codex_thread_id: runtime.slot.codex_thread_id.clone(),
        product_session_id: fixture.product_session_id.clone(),
        work_run_id: None,
        worker_session_id: runtime.slot.worker_session_id.clone(),
    }
}

fn binding_message(
    fixture: &Fixture,
    runtime: &RuntimeAuthority,
    seed: u64,
) -> SessionBindingMessage {
    let ExecutionPortMessage::SessionBindingMessage(mut message) =
        execution_message("session.binding")
    else {
        panic!("session.binding fixture");
    };
    message.attempt = runtime.lease.attempt;
    message.bound_at = at(9);
    message.codex_thread_id = runtime.slot.codex_thread_id.clone();
    message.fencing_token = runtime.lease.fencing_token.clone();
    message.lease = runtime.lease.clone();
    message.lease_id = runtime.lease.lease_id.clone();
    message.message_id = ExecutionMessageId(id("xmsg", seed * 100 + 9));
    message.product_session_id = fixture.product_session_id.clone();
    message.sent_at = at(9);
    message.session_identity = session_identity(fixture, runtime);
    message.source_identity = SessionBindingSourceIdentity {
        kind: SessionBindingSourceIdentityKind::ExecutionWorker,
        lease_id: runtime.lease.lease_id.clone(),
        worker_id: runtime.lease.worker_id.clone(),
        worker_instance_id: runtime.lease.worker_instance_id.clone(),
        worker_session_id: runtime.slot.worker_session_id.clone(),
    };
    message.work_run_id = None;
    message.worker_id = runtime.lease.worker_id.clone();
    message.worker_session_id = runtime.slot.worker_session_id.clone();
    message
}

fn model_open_message(
    fixture: &Fixture,
    runtime: &RuntimeAuthority,
    seed: u64,
) -> ModelOpenMessage {
    let ExecutionPortMessage::ModelOpenMessage(mut message) = execution_message("model.open")
    else {
        panic!("model.open fixture");
    };
    message.lease = runtime.lease.clone();
    message.message_id = ExecutionMessageId(id("xmsg", seed * 100 + 10));
    message.model_exchange_id = winwincode_domain::ModelExchangeId(id("mdl", seed));
    let request = br#"{"prompt":"project the canonical Provider output"}"#;
    message.request.data_base64 = STANDARD.encode(request);
    message.request.payload_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(request)));
    message.request_id = RequestId(id("req", seed * 100 + 10));
    message.sent_at = at(10);
    message.session_identity = session_identity(fixture, runtime);
    message.worker_session_id = runtime.slot.worker_session_id.clone();
    message
}

fn successful_outcome(
    fixture: &Fixture,
    runtime: &RuntimeAuthority,
    seed: u64,
) -> JobOutcomeMessage {
    let ExecutionPortMessage::JobOutcomeMessage(mut message) = execution_message("job.outcome")
    else {
        panic!("job.outcome fixture");
    };
    message.lease = runtime.lease.clone();
    message.message_id = ExecutionMessageId(id("xmsg", seed * 100 + 11));
    message.outcome.artifacts.clear();
    message.outcome.codex_thread_id = Some(runtime.slot.codex_thread_id.clone());
    message.outcome.error = None;
    message.outcome.finished_at = at(20);
    message.outcome.status = ExecutionOutcomeStatus::Succeeded;
    "private Worker summary marker".clone_into(&mut message.outcome.summary);
    message.sent_at = at(20);
    message.session_identity = session_identity(fixture, runtime);
    message.worker_session_id = runtime.slot.worker_session_id.clone();
    message
}

fn runtime_event(fixture: &Fixture, runtime: &RuntimeAuthority, seed: u64) -> RuntimeEventMessage {
    RuntimeEventMessage {
        codex_thread_id: runtime.slot.codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category: ExecutionEventCategory::Lifecycle,
            event_id: ExecutionEventId(id("xevt", seed)),
            occurred_at: at(17),
            payload: None,
            sequence: ExecutionSequence(1),
            summary: "stale predecessor runtime event".to_owned(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: runtime.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at(17),
        session_identity: session_identity(fixture, runtime),
        worker_session_id: runtime.slot.worker_session_id.clone(),
    }
}

fn cancel_message(fixture: &Fixture, runtime: &RuntimeAuthority, seed: u64) -> JobCancelMessage {
    JobCancelMessage {
        kind: JobCancelMessageKind::JobCancel,
        lease: runtime.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", seed)),
        reason: JobCancelMessageReason::Superseded,
        requested_at: at(17),
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at(17),
        session_identity: session_identity(fixture, runtime),
        worker_session_id: runtime.slot.worker_session_id.clone(),
    }
}

fn product_authority_bytes(
    fixture: &mut Fixture,
    job: &ExecutionJob,
    sessions: &[&WorkerSessionId],
) -> Vec<u8> {
    let stream_id = format!(
        "product-sessions:{:x}",
        Sha256::digest(fixture.receipt_scope.as_bytes())
    );
    let product_state = fixture
        .storage
        .load_state(&stream_id)
        .expect("ProductSession state")
        .expect("ProductSession state exists");
    let scope = execution_scope(fixture);
    let queue = fixture
        .storage
        .execution_queue()
        .expect("queue")
        .load_job(&scope, &job.job_id)
        .expect("queue job")
        .expect("queue job exists");
    let lease = fixture
        .storage
        .execution_registry()
        .expect("registry")
        .load_lease(&job.job_id)
        .expect("lease");
    let slots = sessions
        .iter()
        .map(|session| {
            fixture
                .storage
                .worker_session_slots()
                .expect("slots")
                .load(session)
                .expect("slot")
        })
        .collect::<Vec<_>>();
    serde_json::to_vec(&(
        product_state.stream_id,
        product_state.revision,
        product_state.payload,
        queue,
        lease,
        slots,
    ))
    .expect("authority snapshot")
}

fn assert_rejected_without_authority_write(
    fixture: &mut Fixture,
    job: &ExecutionJob,
    sessions: &[&WorkerSessionId],
    message: &ExecutionPortMessage,
) {
    let before = product_authority_bytes(fixture, job, sessions);
    let database_before = terminal_database_snapshot(fixture);
    let result = fixture.accept(message, at(29));
    if let Ok(output) = result {
        assert!(
            !output.is_empty(),
            "stale predecessor ingress was accepted without rejection"
        );
        assert!(
            output.iter().all(|message| match message {
                ExecutionPortMessage::JobOutcomeAckMessage(ack) => !matches!(
                    ack.status,
                    JobOutcomeAckMessageStatus::Accepted | JobOutcomeAckMessageStatus::Duplicate
                ),
                ExecutionPortMessage::RuntimeAckMessage(ack) => !matches!(
                    ack.status,
                    LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
                ),
                ExecutionPortMessage::ModelAckMessage(ack) => !matches!(
                    ack.status,
                    LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
                ),
                _ => false,
            }),
            "stale predecessor ingress unexpectedly returned a successful acknowledgement"
        );
    }
    assert_eq!(
        product_authority_bytes(fixture, job, sessions),
        before,
        "rejected predecessor ingress changed ProductSession, queue, Registry, or slot bytes"
    );
    assert_eq!(
        terminal_database_snapshot(fixture),
        database_before,
        "rejected predecessor ingress changed a business table, receipt, stream, or outbox"
    );
}

fn assert_product_predecessor_is_fenced(
    fixture: &mut Fixture,
    job: &ExecutionJob,
    predecessor: &RuntimeAuthority,
    successor: &RuntimeAuthority,
    seed: u64,
) {
    let old_session_id = predecessor.slot.worker_session_id.clone();
    let new_session_id = successor.slot.worker_session_id.clone();
    let sessions = [&old_session_id, &new_session_id];
    let messages = [
        ExecutionPortMessage::RuntimeEventMessage(runtime_event(fixture, predecessor, seed)),
        ExecutionPortMessage::JobOutcomeMessage(successful_outcome(fixture, predecessor, seed + 1)),
        ExecutionPortMessage::JobCancelMessage(cancel_message(fixture, predecessor, seed + 2)),
    ];
    for message in &messages {
        assert_rejected_without_authority_write(fixture, job, &sessions, message);
    }
}

fn assert_product_replacement_replays_after_expiry(
    fixture: Fixture,
    binding: SessionBindingMessage,
    open: ModelOpenMessage,
    runtime: &RuntimeAuthority,
) {
    let mut fixture = fixture.restart();
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(binding),
            expired(),
        )
        .expect("expired replacement binding replay");
    fixture
        .accept(&model_binding_message(open), expired())
        .expect("expired replacement ModelOpen replay");
    let replayed = ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .expect("replayed ProductSession read")
        .expect("ProductSession");
    assert_eq!(replayed.bindings().len(), 1);
    assert_eq!(replayed.bindings()[0].slot().authority, runtime.slot);
    fixture.close();
}

fn outcome_status(output: &[ExecutionPortMessage]) -> JobOutcomeAckMessageStatus {
    let [ExecutionPortMessage::JobOutcomeAckMessage(message)] = output else {
        panic!("one job.outcome_ack response");
    };
    message.status.clone()
}

fn prepared_provider_fixture(seed: u64) -> (Fixture, ModelOpenMessage) {
    let mut fixture = Fixture::open(seed);
    let job = create_chat_job(&mut fixture, seed);
    let scope = execution_scope(&fixture);
    register_worker(&mut fixture.storage, seed);
    reserve_execution(&mut fixture.storage, &scope, &job, seed);
    transition_job(
        &mut fixture.storage,
        &scope,
        &job.job_id,
        seed * 100 + 20,
        1,
        ExecutionJobState::Queued,
        ExecutionJobState::Leased,
    );
    let runtime = claim_and_open_runtime(&mut fixture, &job, seed);
    accept_dispatch(&mut fixture, &job, &runtime, seed);
    let binding = binding_message(&fixture, &runtime, seed);
    assert!(
        fixture
            .accept(&ExecutionPortMessage::SessionBindingMessage(binding), at(9),)
            .expect("first binding")
            .is_empty()
    );
    let model_open = model_open_message(&fixture, &runtime, seed);
    assert!(
        fixture
            .accept(&model_binding_message(model_open.clone()), at(10),)
            .expect("attach model exchange")
            .iter()
            .any(|message| matches!(message, ExecutionPortMessage::ModelAckMessage(_)))
    );
    configure_model_provider(&mut fixture, seed);
    DurableProviderGatewayIdentitySource::open(&fixture.root)
        .expect("Provider identity source")
        .authorize(&model_open)
        .expect("Provider identity authority");
    (fixture, model_open)
}

#[derive(Clone, Copy, Debug)]
enum PoolAuthorityMutation {
    Request,
    Route,
    Terminal,
}

fn pool_route_fingerprint(route: &Value) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.model-request-route.v1\0");
    for field in [
        "organizationId",
        "projectId",
        "provider",
        "model",
        "credentialReferenceId",
    ] {
        digest.update(
            route[field]
                .as_str()
                .unwrap_or_else(|| panic!("pool route {field}"))
                .as_bytes(),
        );
        digest.update([0]);
    }
    format!("sha256:{:x}", digest.finalize())
}

fn mutate_pool_authority(fixture: &mut Fixture, mutation: PoolAuthorityMutation) {
    let authority = fixture
        .storage
        .provider_exchange_store()
        .expect("Provider exchange store")
        .load_pool_authority()
        .expect("pool authority")
        .expect("durable pool authority");
    let updated_at = authority.updated_at.clone();
    let mut document: Value =
        serde_json::from_slice(authority.state_json()).expect("pool authority JSON");
    let route = &mut document["routes"][0];
    match mutation {
        PoolAuthorityMutation::Request => {
            route["exchanges"][0]["requestId"] = Value::String(id("req", 9_001));
        }
        PoolAuthorityMutation::Route => {
            route["provider"] = Value::String("cross-authority-provider".to_owned());
            let fingerprint = pool_route_fingerprint(route);
            route["exchanges"][0]["routeFingerprint"] = Value::String(fingerprint);
        }
        PoolAuthorityMutation::Terminal => {
            let exchange = &mut route["exchanges"][0];
            exchange["state"] = Value::String("failed".to_owned());
            exchange["terminalOutcome"] = Value::String("failed".to_owned());
            let frames = exchange["frames"]
                .as_array_mut()
                .expect("pool authority frames");
            frames.last_mut().expect("terminal pool frame")["terminalOutcome"] =
                Value::String("failed".to_owned());
        }
    }
    let bytes = serde_json::to_vec(&document).expect("mutated pool authority JSON");
    fixture
        .storage
        .provider_exchange_store()
        .expect("Provider exchange store")
        .save_pool_authority(&bytes, &updated_at)
        .expect("save mutated pool authority");
}

struct AcceptedTerminalFixture {
    fixture: Fixture,
    job: ExecutionJob,
    runtime: RuntimeAuthority,
    binding: SessionBindingMessage,
    outcome: JobOutcomeMessage,
}

fn accepted_terminal_fixture(seed: u64, renewed: bool) -> AcceptedTerminalFixture {
    let mut fixture = Fixture::open(seed);
    let job = create_chat_job(&mut fixture, seed);
    let scope = execution_scope(&fixture);
    register_worker(&mut fixture.storage, seed);
    reserve_execution(&mut fixture.storage, &scope, &job, seed);
    transition_job(
        &mut fixture.storage,
        &scope,
        &job.job_id,
        seed * 100 + 20,
        1,
        ExecutionJobState::Queued,
        ExecutionJobState::Leased,
    );
    let runtime = claim_and_open_runtime(&mut fixture, &job, seed);
    accept_dispatch(&mut fixture, &job, &runtime, seed);
    let binding = binding_message(&fixture, &runtime, seed);
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::SessionBindingMessage(binding.clone()),
                at(9),
            )
            .expect("first binding")
            .is_empty()
    );
    let model_open = model_open_message(&fixture, &runtime, seed);
    assert!(
        fixture
            .accept(&model_binding_message(model_open), at(10),)
            .expect("attach model exchange")
            .iter()
            .any(|message| matches!(message, ExecutionPortMessage::ModelAckMessage(_)))
    );
    let outcome = successful_outcome(&fixture, &runtime, seed);
    if renewed {
        let lease = &outcome.lease;
        assert_eq!(
            fixture
                .storage
                .execution_registry()
                .expect("registry")
                .renew_execution_lease(&winwincode_storage::ExecutionLeaseRenewal {
                    expires_at: at(58),
                    prior_expires_at: lease.expires_at.clone(),
                    job_id: lease.job_id.clone(),
                    lease_id: lease.lease_id.clone(),
                    worker_id: lease.worker_id.clone(),
                    worker_instance_id: lease.worker_instance_id.clone(),
                    attempt: 1,
                    fencing_token: lease.fencing_token.clone(),
                    message_id: ExecutionMessageId(id("xmsg", 99012)),
                    request_id: RequestId(id("req", 99012)),
                    sent_at: at(15),
                })
                .expect("renew before original outcome")
                .status,
            winwincode_storage::LeaseWriteStatus::Accepted
        );
    }
    let output = fixture
        .accept(
            &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
            if renewed { expired() } else { at(20) },
        )
        .expect("first terminal outcome, including delayed delivery after expiry");
    assert_eq!(
        outcome_status(&output),
        JobOutcomeAckMessageStatus::Accepted
    );
    AcceptedTerminalFixture {
        fixture,
        job,
        runtime,
        binding,
        outcome,
    }
}

fn assert_single_binding_and_turn(fixture: &mut Fixture) {
    let replayed = ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .expect("replayed ProductSession read")
        .expect("ProductSession");
    assert_eq!(replayed.messages().len(), 2);
    assert_eq!(replayed.bindings().len(), 1);
}

struct BoundSchedulerFixture {
    fixture: Fixture,
    job: ExecutionJob,
    dispatch: winwincode_execution_port::generated::JobDispatchMessage,
    runtime: RuntimeAuthority,
}

fn bound_scheduler_fixture(seed: u64) -> BoundSchedulerFixture {
    let mut fixture = Fixture::open(seed);
    let job = create_chat_job(&mut fixture, seed);
    let scope = execution_scope(&fixture);
    register_worker(&mut fixture.storage, seed);
    reserve_execution(&mut fixture.storage, &scope, &job, seed);
    finish_bound_scheduler_fixture(fixture, job, seed)
}

fn finish_bound_scheduler_fixture(
    mut fixture: Fixture,
    job: ExecutionJob,
    seed: u64,
) -> BoundSchedulerFixture {
    let dispatch = scheduler_claim(&mut fixture, seed, seed, seed * 200 + 1, "boot-bound", 5);
    let worker_session_id = WorkerSessionId(id("wsn", seed * 200 + 2));
    accept_scheduler_dispatch(
        &mut fixture,
        &dispatch,
        &worker_session_id,
        seed * 200 + 3,
        7,
    );
    let runtime = open_scheduler_runtime(
        &mut fixture,
        &dispatch,
        seed * 200 + 2,
        seed * 200 + 4,
        seed * 200 + 5,
        8,
    );
    let binding = binding_message(&fixture, &runtime, seed * 200 + 6);
    fixture
        .accept(&ExecutionPortMessage::SessionBindingMessage(binding), at(9))
        .expect("bound scheduler SessionBinding");
    let open = model_open_message(&fixture, &runtime, seed * 200 + 7);
    fixture
        .accept(&model_binding_message(open), at(10))
        .expect("bound scheduler ModelOpen");
    BoundSchedulerFixture {
        fixture,
        job,
        dispatch,
        runtime,
    }
}

#[test]
fn failed_terminal_retry_dispatch_rebinds_product_session_and_fences_old_ingress() {
    let seed = 62;
    let BoundSchedulerFixture {
        mut fixture,
        job,
        dispatch,
        runtime: old_runtime,
    } = bound_scheduler_fixture(seed);
    let failed_scope = scheduler_scope(&fixture);
    let failed = RepositoryExecutionScheduler::new(&mut fixture.storage)
        .settle_terminal(&RepositorySchedulerTerminalRequest {
            scope: failed_scope,
            terminal: ExecutionLeaseTerminalRequest {
                job_id: job.job_id.clone(),
                lease_id: dispatch.lease.lease_id.clone(),
                worker_id: dispatch.lease.worker_id.clone(),
                worker_instance_id: dispatch.lease.worker_instance_id.clone(),
                attempt: u64::try_from(dispatch.lease.attempt).expect("attempt"),
                fencing_token: dispatch.lease.fencing_token.clone(),
                outcome: ExecutionLeaseTerminalOutcome::Failed,
                terminal_at: at(11),
                request_id: RequestId(id("req", 12_501)),
            },
        })
        .expect("failed scheduler terminal");
    assert_eq!(failed.job.state, ExecutionJobState::Failed);

    register_worker_process(&mut fixture.storage, seed, seed + 1, 12_502, 12);
    let retry_scope = scheduler_scope(&fixture);
    let replacement_dispatch = RepositoryExecutionScheduler::new(&mut fixture.storage)
        .retry_failed(&RepositorySchedulerRetryRequest {
            scope: retry_scope,
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", 12_504)),
            scheduler_generation: "boot-failed-retry".into(),
            worker_id: WorkerId(id("wrk", seed)),
            worker_instance_id: WorkerInstanceId(id("wki", seed + 1)),
            retryable_failure: true,
            failed_at_tick: 100,
            now_tick: 105,
            policy: SchedulerRetryPolicy {
                max_attempts: 3,
                initial_backoff_ticks: 5,
                max_backoff_ticks: 20,
            },
            issued_at: at(14),
            expires_at: at(40),
        })
        .expect("eligible failed retry")
        .expect("typed failed retry dispatch");
    assert_eq!(replacement_dispatch.job.attempt, 2);
    let replacement_session_id = WorkerSessionId(id("wsn", 12_505));
    accept_scheduler_dispatch(
        &mut fixture,
        &replacement_dispatch,
        &replacement_session_id,
        12_506,
        15,
    );
    let replacement_runtime = open_scheduler_runtime(
        &mut fixture,
        &replacement_dispatch,
        12_505,
        12_507,
        12_508,
        15,
    );
    let mut binding = binding_message(&fixture, &replacement_runtime, 12_509);
    binding.bound_at = at(16);
    binding.sent_at = at(16);
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(binding),
            at(16),
        )
        .expect("retry SessionBinding");
    let mut open = model_open_message(&fixture, &replacement_runtime, 12_510);
    open.sent_at = at(17);
    fixture
        .accept(&model_binding_message(open), at(17))
        .expect("retry ModelOpen");

    let record = ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .expect("ProductSession")
        .expect("ProductSession exists");
    assert_eq!(record.bindings().len(), 1);
    assert_eq!(
        record.bindings()[0].slot().authority,
        replacement_runtime.slot
    );
    assert_product_predecessor_is_fenced(
        &mut fixture,
        &job,
        &old_runtime,
        &replacement_runtime,
        12_511,
    );
    fixture.close();
}

#[test]
fn running_scheduler_replacement_rotates_one_product_session_binding_and_replays_exactly() {
    replacement_continues_react_model_exchanges(ReplacementStart::ModelBound);
}

#[test]
fn replacement_after_session_binding_before_first_model_exchange_starts_original_chat_turn() {
    replacement_continues_react_model_exchanges(ReplacementStart::Unbound);
}

#[test]
fn review_successor_after_output_preparation_continues_multiple_model_exchanges() {
    replacement_continues_react_model_exchanges(ReplacementStart::OutputPreparing);
}

#[test]
fn review_successor_failure_before_model_uses_its_own_terminal_authority() {
    replacement_continues_react_model_exchanges(ReplacementStart::TerminalBeforeModel);
}

#[test]
fn review_successor_cancellation_before_model_preserves_cancellation_authority() {
    replacement_continues_react_model_exchanges(ReplacementStart::CancelledBeforeModel);
}

#[test]
fn review_consecutive_replacement_after_output_preparation_continues_model_exchanges() {
    replacement_continues_react_model_exchanges_chain(
        ReplacementStart::OutputPreparing,
        IntermediateAttempt::BindingStaged,
    );
}

#[test]
fn review_consecutive_replacement_failure_before_model_uses_current_authority() {
    replacement_continues_react_model_exchanges_chain(
        ReplacementStart::TerminalBeforeModel,
        IntermediateAttempt::BindingStaged,
    );
}

#[test]
fn review_consecutive_replacement_failed_before_model_uses_current_authority() {
    replacement_continues_react_model_exchanges_chain(
        ReplacementStart::FailedBeforeModel,
        IntermediateAttempt::BindingStaged,
    );
}

#[test]
fn review_consecutive_replacement_cancellation_before_model_routes_to_current_attempt() {
    replacement_continues_react_model_exchanges_chain(
        ReplacementStart::CancelledBeforeModel,
        IntermediateAttempt::BindingStaged,
    );
}

#[test]
fn review_intermediate_dispatch_without_acceptance_preserves_replacement_lineage() {
    assert_intermediate_replacement_lifecycle(IntermediateAttempt::Dispatched);
}

#[test]
fn review_intermediate_accepted_dispatch_without_slot_preserves_replacement_lineage() {
    assert_intermediate_replacement_lifecycle(IntermediateAttempt::DispatchAccepted);
}

#[test]
fn review_intermediate_slot_without_binding_preserves_replacement_lineage() {
    assert_intermediate_replacement_lifecycle(IntermediateAttempt::SlotOpened);
}

fn assert_intermediate_replacement_lifecycle(intermediate: IntermediateAttempt) {
    for start in [
        ReplacementStart::OutputPreparing,
        ReplacementStart::TerminalBeforeModel,
        ReplacementStart::FailedBeforeModel,
        ReplacementStart::CancelledBeforeModel,
    ] {
        replacement_continues_react_model_exchanges_chain(start, intermediate);
    }
}

#[derive(Clone, Copy)]
enum IntermediateAttempt {
    Absent,
    Dispatched,
    DispatchAccepted,
    SlotOpened,
    BindingStaged,
}

#[derive(Clone, Copy)]
enum ReplacementStart {
    ModelBound,
    Unbound,
    OutputPreparing,
    TerminalBeforeModel,
    FailedBeforeModel,
    CancelledBeforeModel,
}

fn replacement_continues_react_model_exchanges(start: ReplacementStart) {
    replacement_continues_react_model_exchanges_chain(start, IntermediateAttempt::Absent);
}

#[allow(clippy::too_many_lines)]
fn replacement_continues_react_model_exchanges_chain(
    start: ReplacementStart,
    intermediate: IntermediateAttempt,
) {
    let skip_model = !matches!(intermediate, IntermediateAttempt::Absent);
    let original_model_bound = !matches!(start, ReplacementStart::Unbound);
    let seed = match start {
        ReplacementStart::ModelBound => 61,
        ReplacementStart::Unbound => 62,
        ReplacementStart::OutputPreparing => 161,
        ReplacementStart::TerminalBeforeModel => 162,
        ReplacementStart::FailedBeforeModel => 164,
        ReplacementStart::CancelledBeforeModel => 163,
    };
    let mut fixture = Fixture::open(seed);
    let job = create_chat_job(&mut fixture, seed);
    let scope = execution_scope(&fixture);
    register_worker(&mut fixture.storage, seed);
    reserve_execution(&mut fixture.storage, &scope, &job, seed);

    let original_dispatch = scheduler_claim(&mut fixture, seed, seed, 6_101, "boot-old", 5);
    assert!(original_dispatch.replacement_authority.is_none());
    let original_session = WorkerSessionId(id("wsn", 6_102));
    accept_scheduler_dispatch(
        &mut fixture,
        &original_dispatch,
        &original_session,
        6_103,
        7,
    );
    let original_runtime =
        open_scheduler_runtime(&mut fixture, &original_dispatch, 6_102, 6_104, 6_105, 8);
    let original_binding = binding_message(&fixture, &original_runtime, 6_106);
    let mut predecessor_frames = vec![ExecutionPortMessage::SessionBindingMessage(
        original_binding.clone(),
    )];
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(original_binding.clone()),
            at(9),
        )
        .expect("original ProductSession binding");
    if original_model_bound {
        let original_open = model_open_message(&fixture, &original_runtime, 6_107);
        predecessor_frames.push(model_binding_message(original_open.clone()));
        fixture
            .accept(&model_binding_message(original_open), at(10))
            .expect("original ProductSession ModelOpen");
    }

    if matches!(start, ReplacementStart::OutputPreparing) {
        let (open, _) = chat_project_artifact_messages(&fixture, &original_runtime);
        fixture
            .accept(&open, at(13))
            .expect("original ZIP preparing");
        assert!(
            fixture
                .record()
                .messages()
                .iter()
                .any(|message| message.output_preparation.as_deref() == Some("preparing"))
        );
    }
    let original_output_ids: Vec<_> = fixture
        .record()
        .messages()
        .iter()
        .filter(|message| message.state == "streaming")
        .map(|message| message.id.clone())
        .collect();

    register_worker_process(&mut fixture.storage, seed, seed + 1, 6_108, 14);
    let intermediate_runtime = skip_model
        .then(|| {
            let scheduler_scope = scheduler_scope(&fixture);
            let dispatch = RepositoryExecutionScheduler::new(&mut fixture.storage)
                .claim_next(&RepositorySchedulerClaimRequest {
                    scope: scheduler_scope,
                    request_id: RequestId(id("req", 6_120)),
                    scheduler_generation: "boot-intermediate".to_owned(),
                    worker_id: WorkerId(id("wrk", seed)),
                    worker_instance_id: WorkerInstanceId(id("wki", seed + 1)),
                    issued_at: at(25),
                    expires_at: at(26),
                })
                .expect("intermediate scheduler claim")
                .expect("intermediate scheduler dispatch");
            assert_eq!(dispatch.job.attempt, 2);
            if !matches!(intermediate, IntermediateAttempt::Dispatched) {
                accept_scheduler_dispatch(
                    &mut fixture,
                    &dispatch,
                    &WorkerSessionId(id("wsn", 6_121)),
                    6_122,
                    25,
                );
            }
            let runtime = if matches!(
                intermediate,
                IntermediateAttempt::SlotOpened | IntermediateAttempt::BindingStaged
            ) {
                let runtime =
                    open_scheduler_runtime(&mut fixture, &dispatch, 6_121, 6_123, 6_124, 25);
                let mut binding = binding_message(&fixture, &runtime, 6_125);
                binding.bound_at = at(25);
                binding.sent_at = at(25);
                predecessor_frames
                    .push(ExecutionPortMessage::SessionBindingMessage(binding.clone()));
                if matches!(intermediate, IntermediateAttempt::BindingStaged) {
                    fixture
                        .accept(
                            &ExecutionPortMessage::SessionBindingMessage(binding),
                            at(25),
                        )
                        .expect("intermediate binding before any model frame");
                }
                Some(runtime)
            } else {
                None
            };
            register_worker_process(&mut fixture.storage, seed, seed + 2, 6_126, 26);
            runtime
        })
        .flatten();
    let replacement_dispatch = scheduler_claim(
        &mut fixture,
        seed,
        seed + if skip_model { 2 } else { 1 },
        6_110,
        "boot-new",
        if skip_model { 26 } else { 25 },
    );
    assert_eq!(replacement_dispatch.job.job_id, job.job_id);
    assert_eq!(
        replacement_dispatch.job.attempt,
        if skip_model { 3 } else { 2 }
    );
    assert!(replacement_dispatch.replacement_authority.is_some());
    let replacement_session = WorkerSessionId(id("wsn", 6_111));
    accept_scheduler_dispatch(
        &mut fixture,
        &replacement_dispatch,
        &replacement_session,
        6_112,
        26,
    );
    let replacement_runtime =
        open_scheduler_runtime(&mut fixture, &replacement_dispatch, 6_111, 6_113, 6_114, 26);
    let mut replacement_binding = binding_message(&fixture, &replacement_runtime, 6_115);
    replacement_binding.bound_at = at(27);
    replacement_binding.sent_at = at(27);
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(replacement_binding.clone()),
            at(27),
        )
        .expect("replacement ProductSession binding");
    if skip_model {
        for message in &predecessor_frames {
            assert_rejected_without_authority_write(
                &mut fixture,
                &job,
                &[
                    &original_runtime.slot.worker_session_id,
                    &replacement_runtime.slot.worker_session_id,
                ],
                message,
            );
        }
    }
    if matches!(
        start,
        ReplacementStart::TerminalBeforeModel
            | ReplacementStart::FailedBeforeModel
            | ReplacementStart::CancelledBeforeModel
    ) {
        assert_product_predecessor_is_fenced(
            &mut fixture,
            &job,
            &original_runtime,
            &replacement_runtime,
            6_117,
        );
        if let Some(intermediate) = &intermediate_runtime {
            assert_product_predecessor_is_fenced(
                &mut fixture,
                &job,
                intermediate,
                &replacement_runtime,
                6_127,
            );
        }
        let cancelled = matches!(start, ReplacementStart::CancelledBeforeModel);
        if cancelled {
            cancel_bound_chat_at(&mut fixture, 27);
        }
        let mut outcome = successful_outcome(&fixture, &replacement_runtime, seed);
        outcome.outcome.status = ExecutionOutcomeStatus::InfrastructureError;
        outcome.outcome.error = Some(winwincode_execution_port::generated::ExecutionPortError {
            code: ExecutionPortErrorCode::InfrastructureError,
            message: "successor could not start Core".into(),
            retryable: true,
        });
        if matches!(start, ReplacementStart::FailedBeforeModel) {
            outcome.outcome.status = ExecutionOutcomeStatus::Failed;
        }
        if cancelled {
            outcome.outcome.status = ExecutionOutcomeStatus::Cancelled;
            outcome.outcome.error = None;
        }
        outcome.outcome.finished_at = at(28);
        outcome.sent_at = at(28);
        let mut foreign = outcome.clone();
        foreign.session_identity.codex_thread_id = original_runtime.slot.codex_thread_id.clone();
        foreign.outcome.codex_thread_id = Some(original_runtime.slot.codex_thread_id.clone());
        assert_rejected_without_authority_write(
            &mut fixture,
            &job,
            &[
                &original_runtime.slot.worker_session_id,
                &replacement_runtime.slot.worker_session_id,
            ],
            &ExecutionPortMessage::JobOutcomeMessage(foreign),
        );
        if skip_model && matches!(start, ReplacementStart::TerminalBeforeModel) {
            let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
            connection.execute_batch("CREATE TRIGGER fail_consecutive_outcome BEFORE INSERT ON product_state WHEN NEW.stream_id LIKE 'execution-outcome:%' BEGIN SELECT RAISE(ABORT, 'injected consecutive outcome commit failure'); END;").unwrap();
            let before = terminal_database_snapshot(&fixture);
            assert!(
                fixture
                    .accept(
                        &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
                        at(28)
                    )
                    .is_err()
            );
            assert_eq!(
                terminal_database_snapshot(&fixture),
                before,
                "failed atomic acceptance changed product or execution state"
            );
            connection
                .execute_batch("DROP TRIGGER fail_consecutive_outcome")
                .unwrap();
            fixture = fixture.restart();
        }
        let accepted = fixture
            .accept(
                &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
                at(28),
            )
            .expect("successor terminal must use its own staged authority");
        assert_eq!(
            outcome_status(&accepted),
            JobOutcomeAckMessageStatus::Accepted
        );
        assert_replaced_output_is_failed(&mut fixture, &original_output_ids);
        assert_slot_state(
            &mut fixture,
            &replacement_runtime,
            if cancelled {
                WorkerSlotState::Cancelled
            } else {
                WorkerSlotState::Failed
            },
        );
        assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Failed);
        assert_eq!(
            fixture.record().turn_intents()[0]
                .terminal_outcome
                .as_ref()
                .unwrap()
                .status,
            if cancelled {
                ExecutionOutcomeStatus::Cancelled
            } else if matches!(start, ReplacementStart::FailedBeforeModel) {
                ExecutionOutcomeStatus::Failed
            } else {
                ExecutionOutcomeStatus::InfrastructureError
            }
        );
        fixture = fixture.restart();
        let duplicate = fixture
            .accept(&ExecutionPortMessage::JobOutcomeMessage(outcome), expired())
            .unwrap();
        assert_eq!(
            outcome_status(&duplicate),
            JobOutcomeAckMessageStatus::Duplicate
        );
        fixture.close();
        return;
    }
    let mut replacement_open = model_open_message(&fixture, &replacement_runtime, 6_116);
    replacement_open.sent_at = at(28);
    fixture
        .accept(&model_binding_message(replacement_open.clone()), at(28))
        .expect("replacement ProductSession ModelOpen");

    let record = ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .expect("ProductSession read")
        .expect("ProductSession");
    assert_eq!(record.bindings().len(), 1);
    assert_eq!(
        record.bindings()[0].slot().authority,
        replacement_runtime.slot
    );
    assert_eq!(
        record.bindings()[0].model_exchange_id(),
        &replacement_open.model_exchange_id
    );
    assert_eq!(record.turn_intents().len(), 1);

    let mut second_open = model_open_message(&fixture, &replacement_runtime, 6_118);
    second_open.sent_at = at(29);
    fixture
        .accept(&model_binding_message(second_open.clone()), at(29))
        .expect("ReAct next exchange stays on successor binding");
    fixture
        .accept(&model_binding_message(replacement_open.clone()), at(29))
        .expect("replay first exchange keeps original receipt kind");
    fixture
        .accept(&model_binding_message(second_open.clone()), at(29))
        .expect("replay continuation exchange");
    let continued = ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(continued.bindings().len(), 1);
    assert_eq!(
        continued.bindings()[0].slot().authority,
        replacement_runtime.slot
    );
    assert_eq!(
        continued.bindings()[0].model_exchange_id(),
        &second_open.model_exchange_id
    );
    assert!(
        continued
            .messages()
            .iter()
            .all(|message| message.state == "streaming" || message.output_preparation.is_none())
    );
    assert!(continued.bindings()[0].includes_model_exchange(&replacement_open.model_exchange_id));
    assert_eq!(continued.turn_intents().len(), 1);
    assert_replaced_output_is_failed(&mut fixture, &original_output_ids);

    assert_product_predecessor_is_fenced(
        &mut fixture,
        &job,
        &original_runtime,
        &replacement_runtime,
        6_117,
    );
    if let Some(intermediate) = &intermediate_runtime {
        assert_product_predecessor_is_fenced(
            &mut fixture,
            &job,
            intermediate,
            &replacement_runtime,
            6_127,
        );
    }
    assert_product_replacement_replays_after_expiry(
        fixture,
        replacement_binding,
        replacement_open,
        &replacement_runtime,
    );
}

fn assert_replaced_output_is_failed(
    fixture: &mut Fixture,
    ids: &[winwincode_domain::ChatMessageId],
) {
    let record = fixture.record();
    for id in ids {
        let message = record
            .messages()
            .iter()
            .find(|message| &message.id == id)
            .unwrap();
        assert_eq!(message.state, "failed");
        assert!(message.output_preparation.is_none());
    }
}

#[test]
fn accepted_dispatch_without_a_slot_replaces_with_a_fresh_product_session_binding() {
    let seed = 63;
    let mut fixture = Fixture::open(seed);
    let job = create_chat_job(&mut fixture, seed);
    let scope = execution_scope(&fixture);
    register_worker(&mut fixture.storage, seed);
    reserve_execution(&mut fixture.storage, &scope, &job, seed);

    let original = scheduler_claim(&mut fixture, seed, seed, 6_301, "boot-old-no-slot", 5);
    let old_session = WorkerSessionId(id("wsn", 6_302));
    accept_scheduler_dispatch(&mut fixture, &original, &old_session, 6_303, 7);

    register_worker_process(&mut fixture.storage, seed, seed + 1, 6_304, 11);
    let replacement = scheduler_claim(&mut fixture, seed, seed + 1, 6_305, "boot-new-clean", 25);
    let authority = replacement
        .replacement_authority
        .as_ref()
        .expect("typed replacement authority");
    assert!(authority.predecessor_session_identity.is_none());
    let new_session = WorkerSessionId(id("wsn", 6_306));
    accept_scheduler_dispatch(&mut fixture, &replacement, &new_session, 6_307, 26);
    let runtime = open_scheduler_runtime(&mut fixture, &replacement, 6_306, 6_308, 6_309, 26);
    let mut binding = binding_message(&fixture, &runtime, 6_310);
    binding.bound_at = at(27);
    binding.sent_at = at(27);
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(binding),
            at(27),
        )
        .expect("fresh successor SessionBinding");
    let mut open = model_open_message(&fixture, &runtime, 6_311);
    open.sent_at = at(28);
    fixture
        .accept(&model_binding_message(open), at(28))
        .expect("fresh successor ModelOpen");

    let record = ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .expect("ProductSession read")
        .expect("ProductSession");
    assert_eq!(record.bindings().len(), 1);
    assert_eq!(record.bindings()[0].slot().authority, runtime.slot);
    fixture.close();
}

fn assert_queue_state(fixture: &Fixture, job_id: &ExecutionJobId, expected: ExecutionJobState) {
    assert_eq!(
        fixture
            .storage
            .load_execution_job_record(job_id)
            .unwrap()
            .unwrap()
            .state,
        expected
    );
}

fn assert_slot_state(fixture: &mut Fixture, runtime: &RuntimeAuthority, expected: WorkerSlotState) {
    assert_eq!(
        fixture
            .storage
            .worker_session_slots()
            .unwrap()
            .load(&runtime.slot.worker_session_id)
            .unwrap()
            .unwrap()
            .state,
        expected
    );
}

fn execution_resource_snapshot(fixture: &mut Fixture, job_id: &ExecutionJobId) -> Value {
    serde_json::to_value((
        fixture
            .storage
            .load_execution_job_record(job_id)
            .unwrap()
            .unwrap(),
        fixture
            .storage
            .execution_registry()
            .unwrap()
            .load_lease(job_id)
            .unwrap()
            .unwrap(),
        fixture
            .storage
            .execution_admission()
            .unwrap()
            .load_reservation_by_job(job_id)
            .unwrap()
            .unwrap(),
    ))
    .unwrap()
}

fn assert_cancelled_product_outcome(fixture: &mut Fixture, outcome: &JobOutcomeMessage) {
    let record = fixture.record();
    assert_eq!(
        record.session().state(),
        winwincode_session::ProductSessionState::Cancelled
    );
    let terminal = record.turn_intents()[0].terminal_outcome.as_ref().unwrap();
    assert_eq!(terminal.status, ExecutionOutcomeStatus::Cancelled);
    assert_eq!(terminal.usage, outcome.outcome.usage);
    assert_eq!(terminal.finished_at, at(22));
}

fn assert_outcome_revision(fixture: &Fixture, stream: &str, revision: u64) {
    assert_eq!(
        fixture
            .storage
            .load_state(stream)
            .unwrap()
            .unwrap()
            .revision,
        revision
    );
}

fn cancel_bound_chat(fixture: &mut Fixture) {
    cancel_bound_chat_at(fixture, 21);
}

fn cancel_bound_chat_at(fixture: &mut Fixture, second: u64) {
    let current = fixture.record();
    let mut context = command_context(
        &fixture.repository_scope,
        93_201,
        current.session().revision(),
    );
    context.occurred_at = at(second);
    ProductSessionService::new(&mut fixture.storage)
        .cancel_session(&CancelProductSessionCommand {
            context,
            product_session_id: fixture.product_session_id.clone(),
            actor: winwincode_session::AuthenticatedActor::User(UserId(id("usr", 1))),
            reason: "cancel after Worker sealed its raw result".into(),
        })
        .unwrap();
}

fn assert_changed_terminal_frames_rejected(fixture: &mut Fixture, outcome: &JobOutcomeMessage) {
    for change in ["summary", "error", "sentAt"] {
        let mut changed = outcome.clone();
        match change {
            "summary" => changed.outcome.summary.push_str(" changed"),
            "error" => {
                changed.outcome.error =
                    Some(winwincode_execution_port::generated::ExecutionPortError {
                        code: ExecutionPortErrorCode::MessageConflict,
                        message: "changed".into(),
                        retryable: false,
                    });
            }
            _ => changed.sent_at = at(21),
        }
        assert_outcome_conflict_without_write(
            fixture,
            &changed,
            &ExecutionPortErrorCode::MessageConflict,
        );
    }
}

fn terminal_database_snapshot(fixture: &Fixture) -> Vec<(String, Vec<String>)> {
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    let tables = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    tables
        .into_iter()
        .map(|table| {
            let quoted_table = table.replace('"', "\"\"");
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{quoted_table}\""))
                .unwrap();
            let column_count = statement.column_count();
            let mut rows = statement
                .query_map([], |row| {
                    let values = (0..column_count)
                        .map(|index| row.get::<_, rusqlite::types::Value>(index))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(format!("{values:?}"))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows.sort();
            (table, rows)
        })
        .collect()
}

fn assert_outcome_conflict_without_write(
    fixture: &mut Fixture,
    outcome: &JobOutcomeMessage,
    code: &ExecutionPortErrorCode,
) {
    let before = terminal_database_snapshot(fixture);
    let output = fixture
        .accept(
            &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
            expired(),
        )
        .expect("terminal conflict returns a typed acknowledgement");
    let [ExecutionPortMessage::JobOutcomeAckMessage(ack)] = output.as_slice() else {
        panic!("one job.outcome_ack response");
    };
    assert_eq!(ack.status, JobOutcomeAckMessageStatus::RejectedConflict);
    assert_eq!(
        ack.kind,
        winwincode_execution_port::generated::JobOutcomeAckMessageKind::JobOutcomeAck
    );
    assert_eq!(ack.schema_version, outcome.schema_version);
    assert_eq!(ack.lease, outcome.lease);
    assert_eq!(ack.session_identity, outcome.session_identity);
    assert_eq!(ack.worker_session_id, outcome.worker_session_id);
    assert_eq!(ack.sent_at, outcome.sent_at);
    let error = ack.error.as_ref().expect("typed rejection reason");
    assert_eq!(&error.code, code);
    assert!(!error.retryable);
    assert_eq!(
        terminal_database_snapshot(fixture),
        before,
        "terminal conflict must leave every durable table unchanged"
    );
    let replay = fixture
        .accept(
            &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
            expired(),
        )
        .unwrap();
    assert_eq!(
        replay, output,
        "repeated rejection preserves the entire acknowledgement identity"
    );
    assert_eq!(terminal_database_snapshot(fixture), before);
}

#[test]
fn cancellation_freezes_one_product_decision_without_changing_the_original_worker_outcome() {
    for status in [
        ExecutionOutcomeStatus::Succeeded,
        ExecutionOutcomeStatus::Failed,
        ExecutionOutcomeStatus::InfrastructureError,
    ] {
        for unknown in [false, true] {
            let seed = 93;
            let BoundSchedulerFixture {
                mut fixture,
                job,
                runtime,
                ..
            } = bound_scheduler_fixture(seed);
            let mut outcome = successful_outcome(&fixture, &runtime, seed);
            outcome.outcome.status = status.clone();
            if unknown {
                outcome.outcome.usage = Some(
                    winwincode_execution_port::generated::ExecutionOutcomeUsage::unknown(
                        12_000, 17,
                    ),
                );
            }
            let original = serde_json::to_value(&outcome).unwrap();
            cancel_bound_chat(&mut fixture);
            let cancelling = fixture
                .storage
                .load_execution_job_record(&job.job_id)
                .unwrap()
                .unwrap();
            assert_eq!(cancelling.state, ExecutionJobState::Cancelling);
            let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
            connection.execute_batch("CREATE TRIGGER fail_cancelled_close BEFORE UPDATE ON worker_session_slots WHEN NEW.state='cancelled' BEGIN SELECT RAISE(ABORT, 'injected terminal cleanup failure'); END;").unwrap();
            let message = ExecutionPortMessage::JobOutcomeMessage(outcome.clone());
            let cleanup_error = fixture.accept(&message, at(22)).unwrap_err();
            let stream = format!("execution-outcome:{}:1", job.job_id.0);
            let stored = fixture
                .storage
                .load_state(&stream)
                .unwrap()
                .unwrap_or_else(|| panic!("terminal was not accepted: {cleanup_error:?}"));
            assert_eq!(stored.revision, 1);
            let retained: Value = serde_json::from_slice(&stored.payload).unwrap();
            assert_eq!(retained["message"], original);
            assert_eq!(retained["decision"]["status"], "cancelled");
            assert_eq!(
                retained["decision"]["terminalAt"],
                serde_json::to_value(at(22)).unwrap()
            );
            assert_eq!(
                retained["decision"]["cancellation"],
                serde_json::to_value(&cancelling.cancellation).unwrap()
            );
            assert_cancelled_product_outcome(&mut fixture, &outcome);
            let lease_outcome: String = connection
                .query_row(
                    "SELECT outcome FROM execution_lease_terminals WHERE lease_id=?1",
                    [&runtime.lease.lease_id.0],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(lease_outcome, "cancelled");
            connection
                .execute_batch("DROP TRIGGER fail_cancelled_close")
                .unwrap();
            drop(connection);
            fixture = fixture.restart();
            assert_eq!(
                outcome_status(&fixture.accept(&message, expired()).unwrap()),
                JobOutcomeAckMessageStatus::Duplicate
            );
            let ready = fixture.storage.load_state(&stream).unwrap().unwrap();
            assert_eq!(ready.revision, 2);
            let recovered: Value = serde_json::from_slice(&ready.payload).unwrap();
            assert_eq!(recovered["message"], original);
            assert_eq!(recovered["decision"], retained["decision"]);
            assert_slot_state(&mut fixture, &runtime, WorkerSlotState::Cancelled);
            let mut changed = outcome;
            changed.outcome.finished_at = at(21);
            assert_outcome_conflict_without_write(
                &mut fixture,
                &changed,
                &ExecutionPortErrorCode::MessageConflict,
            );
            assert_eq!(fixture.storage.load_state(&stream).unwrap().unwrap(), ready);
            fixture.close();
        }
    }
}

#[test]
fn unstarted_cancelled_chat_rejects_failure_when_queue_cancellation_is_pending() {
    let seed = 92;
    let mut fixture = Fixture::open(seed);
    let job = create_chat_job(&mut fixture, seed);
    let scope = execution_scope(&fixture);
    register_worker(&mut fixture.storage, seed);
    reserve_execution(&mut fixture.storage, &scope, &job, seed);
    let dispatch = scheduler_claim(&mut fixture, seed, seed, 92_201, "boot-unstarted-cancel", 5);
    let worker_session_id = WorkerSessionId(id("wsn", 92_202));
    accept_scheduler_dispatch(&mut fixture, &dispatch, &worker_session_id, 92_203, 7);
    let runtime = open_scheduler_runtime(&mut fixture, &dispatch, 92_202, 92_204, 92_205, 8);
    let binding = binding_message(&fixture, &runtime, 92_206);
    fixture
        .accept(&ExecutionPortMessage::SessionBindingMessage(binding), at(9))
        .unwrap();
    let before = fixture.record();
    let mut context = command_context(
        &fixture.repository_scope,
        92_207,
        before.session().revision(),
    );
    context.occurred_at = at(11);
    let cancel = CancelProductSessionCommand {
        context,
        product_session_id: fixture.product_session_id.clone(),
        actor: winwincode_session::AuthenticatedActor::User(UserId(id("usr", 1))),
        reason: "cancel before model open".into(),
    };
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_queue_cancel BEFORE UPDATE ON scheduler_execution_jobs
         WHEN NEW.state='cancelling'
         BEGIN SELECT RAISE(ABORT, 'injected queue cancellation failure'); END;",
        )
        .unwrap();
    assert!(
        ProductSessionService::new(&mut fixture.storage)
            .cancel_session(&cancel)
            .is_err()
    );
    let cancelled = fixture.record();
    assert_eq!(
        cancelled.session().state(),
        winwincode_session::ProductSessionState::Cancelled
    );
    assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Running);
    let mut outcome = successful_outcome(&fixture, &runtime, seed);
    outcome.outcome.status = ExecutionOutcomeStatus::InfrastructureError;
    outcome.outcome.usage =
        Some(winwincode_execution_port::generated::ExecutionOutcomeUsage::unknown(12_000, 0));
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
                at(20)
            )
            .is_err()
    );
    assert_eq!(fixture.record(), cancelled);
    assert!(
        fixture
            .storage
            .load_state(&format!("execution-outcome:{}:1", job.job_id.0))
            .unwrap()
            .is_none()
    );
    connection
        .execute_batch("DROP TRIGGER fail_queue_cancel")
        .unwrap();
    drop(connection);
    ProductSessionService::new(&mut fixture.storage)
        .cancel_session(&cancel)
        .unwrap();
    outcome.outcome.status = ExecutionOutcomeStatus::Cancelled;
    let message = ExecutionPortMessage::JobOutcomeMessage(outcome);
    assert_eq!(
        outcome_status(&fixture.accept(&message, at(20)).unwrap()),
        JobOutcomeAckMessageStatus::Accepted
    );
    fixture = fixture.restart();
    assert_eq!(
        outcome_status(&fixture.accept(&message, expired()).unwrap()),
        JobOutcomeAckMessageStatus::Duplicate
    );
    assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Failed);
    fixture.close();
}

#[test]
fn cancelled_chat_preserves_unknown_usage_and_cancelled_release_reason() {
    let seed = 91;
    let BoundSchedulerFixture {
        mut fixture,
        job,
        runtime,
        ..
    } = bound_scheduler_fixture(seed);
    let mut outcome = successful_outcome(&fixture, &runtime, seed);
    outcome.outcome.status = ExecutionOutcomeStatus::Cancelled;
    let usage = winwincode_execution_port::generated::ExecutionOutcomeUsage::unknown(12_000, 17);
    outcome.outcome.usage = Some(usage.clone());
    let message = ExecutionPortMessage::JobOutcomeMessage(outcome);
    assert_eq!(
        outcome_status(&fixture.accept(&message, at(20)).unwrap()),
        JobOutcomeAckMessageStatus::Accepted
    );
    fixture = fixture.restart();
    assert_eq!(
        outcome_status(&fixture.accept(&message, expired()).unwrap()),
        JobOutcomeAckMessageStatus::Duplicate
    );
    let reservation = fixture
        .storage
        .execution_admission()
        .unwrap()
        .load_reservation_by_job(&job.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, ExecutionReservationState::Released);
    assert_eq!(
        reservation.release_reason,
        Some(winwincode_storage::ExecutionReservationReleaseReason::Cancelled)
    );
    assert_eq!(reservation.actual_tokens, None);
    assert_eq!(reservation.actual_runtime_millis, None);
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    let stored: String = connection
        .query_row(
            "SELECT terminal_usage_json FROM execution_admission_reconciliation WHERE job_id=?1",
            [&job.job_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<winwincode_execution_port::generated::ExecutionOutcomeUsage>(
            &stored
        )
        .unwrap(),
        usage
    );
    drop(connection);
    fixture.close();
}

fn accepted_outcome_retry(
    fixture: &Fixture,
    job_id: &ExecutionJobId,
    seed: u64,
) -> RepositorySchedulerRetryRequest {
    RepositorySchedulerRetryRequest {
        scope: scheduler_scope(fixture),
        job_id: job_id.clone(),
        request_id: RequestId(id("req", 90_202)),
        scheduler_generation: "boot-accepted-outcome-retry".into(),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", seed + 1)),
        retryable_failure: true,
        failed_at_tick: 100,
        now_tick: 105,
        policy: SchedulerRetryPolicy {
            max_attempts: 3,
            initial_backoff_ticks: 5,
            max_backoff_ticks: 20,
        },
        issued_at: at(23),
        expires_at: at(50),
    }
}

#[test]
fn accepted_failed_outcome_blocks_retry_until_cleanup_is_acknowledgement_ready() {
    let seed = 90;
    let BoundSchedulerFixture {
        mut fixture,
        job,
        runtime,
        ..
    } = bound_scheduler_fixture(seed);
    let mut outcome = successful_outcome(&fixture, &runtime, seed);
    outcome.outcome.status = ExecutionOutcomeStatus::InfrastructureError;
    outcome.outcome.error = Some(winwincode_execution_port::generated::ExecutionPortError {
        code: ExecutionPortErrorCode::InfrastructureError,
        message: "original infrastructure failure".into(),
        retryable: true,
    });
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    connection.execute_batch(
        "CREATE TRIGGER fail_terminal_accounting BEFORE UPDATE ON execution_admission_reservations
         WHEN NEW.state IN ('settled', 'released')
         BEGIN SELECT RAISE(ABORT, 'injected terminal accounting failure'); END;"
    ).unwrap();
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
                at(20)
            )
            .is_err()
    );
    let stream = format!("execution-outcome:{}:1", job.job_id.0);
    assert_outcome_revision(&fixture, &stream, 1);
    let original_resources = execution_resource_snapshot(&mut fixture, &job.job_id);
    assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Failed);
    assert_eq!(original_resources[2]["state"], "running");
    register_worker_process(&mut fixture.storage, seed, seed + 1, 90_201, 22);
    let retry = accepted_outcome_retry(&fixture, &job.job_id, seed);
    assert!(
        RepositoryExecutionScheduler::new(&mut fixture.storage)
            .retry_failed(&retry)
            .is_err()
    );
    assert_eq!(
        execution_resource_snapshot(&mut fixture, &job.job_id),
        original_resources
    );
    connection
        .execute_batch("DROP TRIGGER fail_terminal_accounting")
        .unwrap();
    drop(connection);
    fixture = fixture.restart();
    let replay = fixture
        .accept(
            &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
            at(23),
        )
        .unwrap();
    assert_eq!(
        outcome_status(&replay),
        JobOutcomeAckMessageStatus::Duplicate
    );
    assert_outcome_revision(&fixture, &stream, 2);
    let settled = fixture
        .storage
        .execution_admission()
        .unwrap()
        .load_reservation_by_job(&job.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(settled.state, ExecutionReservationState::Settled);
    let successor = RepositoryExecutionScheduler::new(&mut fixture.storage)
        .retry_failed(&retry)
        .unwrap()
        .unwrap();
    assert_eq!(successor.job.attempt, 2);
    let successor_resources = execution_resource_snapshot(&mut fixture, &job.job_id);
    let duplicate = fixture
        .accept(&ExecutionPortMessage::JobOutcomeMessage(outcome), expired())
        .unwrap();
    assert_eq!(
        outcome_status(&duplicate),
        JobOutcomeAckMessageStatus::Duplicate
    );
    assert_eq!(
        execution_resource_snapshot(&mut fixture, &job.job_id),
        successor_resources
    );
    assert_eq!(
        successor_resources[2],
        serde_json::to_value(&settled).unwrap()
    );
    fixture.close();
}

#[test]
#[allow(clippy::too_many_lines)]
fn review_outcome_recovery_pages_retry_failed_cleanup_without_worker_replay() {
    let mut fixture = Fixture::open(180);
    fixture.storage.worker_session_slots().unwrap();
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_page_cleanup BEFORE UPDATE ON worker_session_slots
        WHEN NEW.state='completed' BEGIN SELECT RAISE(ABORT, 'cleanup temporarily unavailable'); END;").unwrap();
    let mut originals = Vec::new();
    for seed in 180..245 {
        fixture.repository_scope.organization_id = OrganizationId(id("org", seed));
        fixture.repository_scope.workspace_id = WorkspaceId(id("wsp", seed));
        fixture.repository_scope.project_id = ProjectId(id("prj", seed));
        fixture.repository_scope.repository_id = RepositoryId(id("rep", seed));
        fixture.receipt_scope =
            receipt_scope_key(&public_repository_scope(&fixture.repository_scope)).unwrap();
        fixture.product_session_id = ProductSessionId(id("psn", seed));
        let job = create_chat_job(&mut fixture, seed);
        let scope = execution_scope(&fixture);
        register_worker(&mut fixture.storage, seed);
        reserve_execution(&mut fixture.storage, &scope, &job, seed);
        let BoundSchedulerFixture {
            fixture: bound,
            job,
            runtime,
            ..
        } = finish_bound_scheduler_fixture(fixture, job, seed);
        fixture = bound;
        let outcome = successful_outcome(&fixture, &runtime, seed);
        assert!(
            fixture
                .accept(
                    &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
                    at(20)
                )
                .is_err()
        );
        let stream = format!("execution-outcome:{}:1", job.job_id.0);
        assert_outcome_revision(&fixture, &stream, 1);
        originals.push((stream, outcome, runtime.slot.worker_session_id));
    }
    // A failing row cannot starve later pages or manufacture a ready receipt.
    assert_eq!(
        fixture
            .control_plane
            .recover_accepted_execution_outcomes()
            .unwrap(),
        0
    );
    assert_eq!(
        fixture
            .control_plane
            .recover_accepted_execution_outcomes()
            .unwrap(),
        0
    );
    for (stream, _, _) in &originals {
        assert_outcome_revision(&fixture, stream, 1);
    }
    connection
        .execute_batch("DROP TRIGGER fail_page_cleanup")
        .unwrap();
    let first = fixture
        .control_plane
        .recover_accepted_execution_outcomes()
        .unwrap();
    let second = fixture
        .control_plane
        .recover_accepted_execution_outcomes()
        .unwrap();
    assert_eq!((first, second), (64, 1));
    let mut retained = Vec::new();
    for (stream, outcome, worker_session_id) in originals {
        assert_outcome_revision(&fixture, &stream, 2);
        let stored = fixture.storage.load_state(&stream).unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&stored.payload).unwrap()["message"],
            serde_json::to_value(outcome).unwrap()
        );
        assert_eq!(
            fixture
                .storage
                .worker_session_slots()
                .unwrap()
                .load(&worker_session_id)
                .unwrap()
                .unwrap()
                .state,
            WorkerSlotState::Completed
        );
        retained.push((stream, stored));
    }
    assert_eq!(
        fixture
            .control_plane
            .recover_accepted_execution_outcomes()
            .unwrap(),
        0
    );
    assert_eq!(
        fixture
            .control_plane
            .recover_accepted_execution_outcomes()
            .unwrap(),
        0
    );
    for (stream, stored) in retained {
        assert_eq!(fixture.storage.load_state(&stream).unwrap(), Some(stored));
    }
    drop(connection);
    fixture.close();
}

#[test]
fn review_startup_finishes_accepted_outcome_without_old_worker_replay() {
    let seed = 174;
    let BoundSchedulerFixture {
        mut fixture,
        job,
        runtime,
        ..
    } = bound_scheduler_fixture(seed);
    let outcome = successful_outcome(&fixture, &runtime, seed);
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_cleanup BEFORE UPDATE ON worker_session_slots
        WHEN NEW.state='completed' BEGIN SELECT RAISE(ABORT, 'cleanup fault'); END;",
        )
        .unwrap();
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
                at(20)
            )
            .is_err()
    );
    let stream = format!("execution-outcome:{}:1", job.job_id.0);
    assert_outcome_revision(&fixture, &stream, 1);
    assert_slot_state(&mut fixture, &runtime, WorkerSlotState::Running);
    register_worker_process(&mut fixture.storage, seed, seed + 1, 174_201, 22);
    connection
        .execute_batch("DROP TRIGGER fail_cleanup")
        .unwrap();
    drop(connection);
    fixture = fixture.restart();
    assert_outcome_revision(&fixture, &stream, 2);
    assert_slot_state(&mut fixture, &runtime, WorkerSlotState::Completed);
    let retained = fixture.storage.load_state(&stream).unwrap().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&retained.payload).unwrap()["message"],
        serde_json::to_value(outcome).unwrap()
    );
    let before = execution_resource_snapshot(&mut fixture, &job.job_id);
    let slot = fixture
        .storage
        .worker_session_slots()
        .unwrap()
        .load(&runtime.slot.worker_session_id)
        .unwrap();
    fixture = fixture.restart();
    assert_eq!(fixture.storage.load_state(&stream).unwrap(), Some(retained));
    assert_eq!(
        execution_resource_snapshot(&mut fixture, &job.job_id),
        before
    );
    assert_eq!(
        fixture
            .storage
            .worker_session_slots()
            .unwrap()
            .load(&runtime.slot.worker_session_id)
            .unwrap(),
        slot
    );
    fixture.close();
}

#[test]
fn chat_terminal_commit_survives_cleanup_failure_and_replays_the_full_original_frame() {
    let seed = 74;
    let BoundSchedulerFixture {
        mut fixture,
        job,
        runtime,
        ..
    } = bound_scheduler_fixture(seed);
    let outcome = successful_outcome(&fixture, &runtime, seed);
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_outcome_slot_close BEFORE UPDATE ON worker_session_slots
         WHEN NEW.state='completed'
         BEGIN SELECT RAISE(ABORT, 'injected cleanup failure'); END;",
        )
        .unwrap();
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
                at(20)
            )
            .is_err()
    );
    let outcome_stream = format!("execution-outcome:{}:1", job.job_id.0);
    let accepted = fixture
        .storage
        .load_state(&outcome_stream)
        .unwrap()
        .unwrap();
    assert_eq!(accepted.revision, 1);
    let retained: Value = serde_json::from_slice(&accepted.payload).unwrap();
    assert_eq!(retained["message"], serde_json::to_value(&outcome).unwrap());
    assert_eq!(retained["acknowledgementReady"], false);
    assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Completed);
    let record = ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        record
            .messages()
            .iter()
            .find(|message| message.role == "assistant")
            .unwrap()
            .state,
        "completed"
    );
    assert_slot_state(&mut fixture, &runtime, WorkerSlotState::Running);
    connection
        .execute_batch("DROP TRIGGER fail_outcome_slot_close")
        .unwrap();
    drop(connection);
    fixture = fixture.restart();
    let output = fixture
        .accept(
            &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
            expired(),
        )
        .unwrap();
    assert_eq!(
        outcome_status(&output),
        JobOutcomeAckMessageStatus::Duplicate
    );
    let ready = fixture
        .storage
        .load_state(&outcome_stream)
        .unwrap()
        .unwrap();
    assert_eq!(ready.revision, 2);
    assert_eq!(
        serde_json::from_slice::<Value>(&ready.payload).unwrap()["acknowledgementReady"],
        true
    );
    assert_slot_state(&mut fixture, &runtime, WorkerSlotState::Completed);
    assert_changed_terminal_frames_rejected(&mut fixture, &outcome);
    register_worker_process(&mut fixture.storage, seed, seed + 1, 74_201, 22);
    let replay = fixture
        .accept(&ExecutionPortMessage::JobOutcomeMessage(outcome), expired())
        .unwrap();
    assert_eq!(
        outcome_status(&replay),
        JobOutcomeAckMessageStatus::Duplicate
    );
    assert_eq!(
        fixture
            .storage
            .load_state(&outcome_stream)
            .unwrap()
            .unwrap(),
        ready
    );
    assert_single_binding_and_turn(&mut fixture);
    fixture.close();
}

#[test]
fn renewed_chat_terminal_keeps_original_receipt_after_expired_restart() {
    let AcceptedTerminalFixture {
        mut fixture,
        outcome,
        ..
    } = accepted_terminal_fixture(32, true);
    fixture = fixture.restart();
    let replay = fixture
        .accept(
            &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
            expired(),
        )
        .expect("original terminal replay after renewal and expiry");
    assert_eq!(
        outcome_status(&replay),
        JobOutcomeAckMessageStatus::Duplicate
    );
    let mut changed = outcome.clone();
    changed.outcome.last_event_sequence.0 += 1;
    assert_outcome_conflict_without_write(
        &mut fixture,
        &changed,
        &ExecutionPortErrorCode::MessageConflict,
    );
    let mut fresh = outcome.clone();
    fresh.message_id = ExecutionMessageId(id("xmsg", 99013));
    assert_outcome_conflict_without_write(
        &mut fixture,
        &fresh,
        &ExecutionPortErrorCode::MessageConflict,
    );
    let mut unaccepted = outcome;
    unaccepted.lease.expires_at = at(49);
    assert_outcome_conflict_without_write(
        &mut fixture,
        &unaccepted,
        &ExecutionPortErrorCode::JobDispatchConflict,
    );
    assert_single_binding_and_turn(&mut fixture);
    fixture.close();
}

#[test]
fn terminal_resource_close_then_expired_restart_keeps_binding_and_outcome_replay_exact() {
    let seed = 31;
    let AcceptedTerminalFixture {
        mut fixture,
        job,
        runtime,
        binding,
        outcome,
    } = accepted_terminal_fixture(seed, false);

    let record = ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .expect("ProductSession terminal read")
        .expect("ProductSession");
    let assistant = record
        .messages()
        .iter()
        .find(|message| message.role == "assistant")
        .expect("assistant terminal message");
    assert_eq!(assistant.content, "");
    assert!(!assistant.content.contains("private Worker summary marker"));
    assert_eq!(assistant.state, "completed");
    assert_eq!(
        record.turn_intents()[0].terminal_outcome.as_ref(),
        Some(
            &winwincode_control_plane::ProductSessionTurnTerminalOutcome {
                artifact_refs: None,
                status: outcome.outcome.status.clone(),
                usage: outcome.outcome.usage.clone(),
                last_event_sequence: outcome.outcome.last_event_sequence.clone(),
                finished_at: outcome.outcome.finished_at.clone(),
            }
        )
    );
    assert_eq!(
        fixture
            .storage
            .execution_admission()
            .expect("admission")
            .load_reservation_by_job(&job.job_id)
            .expect("reservation")
            .expect("reservation record")
            .state,
        ExecutionReservationState::Settled
    );
    assert_eq!(
        fixture
            .storage
            .worker_session_slots()
            .expect("slots")
            .load(&runtime.slot.worker_session_id)
            .expect("slot load")
            .expect("slot record")
            .state,
        WorkerSlotState::Completed
    );

    fixture = fixture.restart();
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::SessionBindingMessage(binding.clone()),
                expired(),
            )
            .expect("expired exact binding replay")
            .is_empty()
    );
    let duplicate = fixture
        .accept(
            &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
            expired(),
        )
        .expect("expired exact terminal replay");
    assert_eq!(
        outcome_status(&duplicate),
        JobOutcomeAckMessageStatus::Duplicate
    );

    let mut changed = binding.clone();
    changed.bound_at = at(10);
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::SessionBindingMessage(changed),
                expired(),
            )
            .is_err(),
        "changed binding body must conflict with its stored receipt"
    );
    let mut forged_old_time = binding;
    forged_old_time.message_id = ExecutionMessageId(id("xmsg", seed * 100 + 30));
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::SessionBindingMessage(forged_old_time),
                expired(),
            )
            .is_err(),
        "a new message cannot use Worker sentAt to bypass trusted lease expiry"
    );

    assert_single_binding_and_turn(&mut fixture);
    fixture.close();
}

#[test]
fn sealed_provider_batch_projects_and_changed_chunks_leave_chat_and_pending_untouched() {
    let (mut fixture, model_open) = prepared_provider_fixture(41);
    let mut application = model_application(&fixture);
    let gateway = provider_open(&mut application, &model_open);
    let batch = application
        .complete_loopback_before_product_session_projection_for_test(&gateway, &at(12))
        .expect("durable sealed Provider batch");
    assert_eq!(assistant_content(&mut fixture), "");
    application
        .project_product_session_batch_for_test(&batch)
        .expect("project authentic sealed Provider batch");
    assert_eq!(
        assistant_content(&mut fixture),
        "WinWinCode deterministic loopback response"
    );
    drop(application);
    fixture.close();

    let (mut tampered_fixture, model_open) = prepared_provider_fixture(42);
    let mut application = model_application(&tampered_fixture);
    let gateway = provider_open(&mut application, &model_open);
    let mut batch = application
        .complete_loopback_before_product_session_projection_for_test(&gateway, &at(12))
        .expect("durable sealed Provider batch");
    batch.corrupt_chunks_for_test();
    assert!(
        application
            .project_product_session_batch_for_test(&batch)
            .is_err(),
        "changed chunks must fail the private batch seal"
    );
    assert_eq!(assistant_content(&mut tampered_fixture), "");
    assert!(
        tampered_fixture
            .storage
            .load_state("product-session-provider-frame-pending:v1")
            .expect("pending Provider catalog")
            .is_none(),
        "a changed batch cannot create pending projection state"
    );
    drop(application);
    tampered_fixture.close();
}

#[test]
fn restart_recovers_durable_provider_pool_batch_before_pending_projection() {
    let (mut fixture, model_open) = prepared_provider_fixture(43);
    let mut application = model_application(&fixture);
    let gateway = provider_open(&mut application, &model_open);
    let batch = application
        .complete_loopback_before_product_session_projection_for_test(&gateway, &at(12))
        .expect("durable Provider batch before projection");
    assert!(!batch.chunks.is_empty());
    assert_eq!(assistant_content(&mut fixture), "");
    assert!(
        fixture
            .storage
            .load_state("product-session-provider-frame-pending:v1")
            .expect("pending Provider catalog")
            .is_none(),
        "the fault point is before ProductSession pending state"
    );
    drop(application);

    let restarted = model_application(&fixture);
    assert_eq!(
        assistant_content(&mut fixture),
        "WinWinCode deterministic loopback response"
    );
    drop(restarted);
    let exact_restart = model_application(&fixture);
    assert_eq!(
        assistant_content(&mut fixture),
        "WinWinCode deterministic loopback response"
    );
    drop(exact_restart);
    fixture.close();
}

#[test]
fn restart_rejects_cross_authority_pool_request_route_and_terminal_facts() {
    for (offset, mutation) in [
        PoolAuthorityMutation::Request,
        PoolAuthorityMutation::Route,
        PoolAuthorityMutation::Terminal,
    ]
    .into_iter()
    .enumerate()
    {
        let seed = 50 + u64::try_from(offset).expect("mutation seed");
        let (mut fixture, model_open) = prepared_provider_fixture(seed);
        let mut application = model_application(&fixture);
        let gateway = provider_open(&mut application, &model_open);
        application
            .complete_loopback_before_product_session_projection_for_test(&gateway, &at(12))
            .expect("durable Provider batch before authority mutation");
        drop(application);
        mutate_pool_authority(&mut fixture, mutation);
        assert!(
            try_model_application(&fixture).is_err(),
            "{mutation:?} must not be rebound to the frozen Gateway exchange"
        );
        assert_eq!(assistant_content(&mut fixture), "");
        assert!(
            fixture
                .storage
                .load_state("product-session-provider-frame-pending:v1")
                .expect("pending Provider catalog")
                .is_none(),
            "cross-authority recovery must write neither pending state nor Chat"
        );
        fixture.close();
    }
}

fn model_binding_message(open: ModelOpenMessage) -> ExecutionPortMessage {
    let ExecutionPortMessage::ModelChunkMessage(mut chunk) = execution_message("model.chunk")
    else {
        panic!("chunk fixture")
    };
    chunk.lease = open.lease;
    chunk.message_id = open.message_id;
    chunk.worker_session_id = open.worker_session_id;
    chunk.session_identity = open.session_identity;
    chunk.model_exchange_id = open.model_exchange_id;
    chunk.sequence = ExecutionSequence(1);
    chunk.payload = None;
    chunk.error = None;
    chunk.is_final = false;
    chunk.sent_at = open.sent_at;
    ExecutionPortMessage::ModelChunkMessage(chunk)
}

#[test]
fn device_model_calls_share_one_execution_binding_and_replay_after_restart() {
    let (mut fixture, first) = prepared_provider_fixture(91);
    let mut second = first.clone();
    second.model_exchange_id = winwincode_domain::ModelExchangeId(id("mdl", 9191));
    second.message_id = ExecutionMessageId(id("xmsg", 9191));
    let second_frame = model_binding_message(second.clone());
    fixture
        .accept(&second_frame, at(11))
        .expect("second tool-loop model call");
    fixture = fixture.restart();
    fixture
        .accept(&second_frame, at(12))
        .expect("second call replay");
    fixture
        .accept(&model_binding_message(first.clone()), at(13))
        .expect("first call replay");
    let record = ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .expect("read")
        .expect("session");
    assert_eq!(record.bindings().len(), 1);
    let binding = &record.bindings()[0];
    assert!(binding.includes_model_exchange(&first.model_exchange_id));
    assert!(binding.includes_model_exchange(&second.model_exchange_id));
    assert_eq!(binding.model_exchange_id(), &second.model_exchange_id);
    let mut foreign = second;
    foreign.worker_session_id = WorkerSessionId(id("wsn", 99999));
    assert!(
        fixture
            .accept(&model_binding_message(foreign), at(14))
            .is_err()
    );
    fixture.close();
}

#[test]
fn bound_chat_empty_frame_rejects_foreign_thread_identity() {
    let (mut fixture, first) = prepared_provider_fixture(94);
    let ExecutionPortMessage::ModelChunkMessage(mut chunk) = model_binding_message(first) else {
        panic!("chunk");
    };
    chunk.session_identity.codex_thread_id = CodexThreadId(id("cdx", 94949));
    assert!(
        fixture
            .accept(&ExecutionPortMessage::ModelChunkMessage(chunk), at(11))
            .is_err()
    );
    fixture.close();
}

#[test]
fn bound_chat_accepts_delayed_frames_and_replays_exact_frames_after_expiry() {
    let (mut fixture, first) = prepared_provider_fixture(95);
    let ExecutionPortMessage::ModelChunkMessage(mut chunk) = model_binding_message(first.clone())
    else {
        panic!("chunk");
    };
    chunk.sequence = ExecutionSequence(2);
    chunk.message_id = ExecutionMessageId(id("xmsg", 95952));
    let body = br#"{"delta":"accepted","type":"output_text_delta"}"#;
    chunk.payload = Some(winwincode_execution_port::generated::EncodedPayload {
        content_type: "application/json".into(),
        data_base64: STANDARD.encode(body),
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(body))),
    });
    let accepted = ExecutionPortMessage::ModelChunkMessage(chunk.clone());
    fixture
        .accept(&accepted, at(11))
        .expect("first seen under live lease");
    fixture = fixture.restart();
    fixture
        .accept(&accepted, expired())
        .expect("exact retained frame after expiry");
    chunk.sequence = ExecutionSequence(3);
    chunk.message_id = ExecutionMessageId(id("xmsg", 95953));
    fixture
        .accept(&ExecutionPortMessage::ModelChunkMessage(chunk), expired())
        .expect("an existing exchange can deliver another retained output frame after expiry");
    assert_eq!(assistant_content(&mut fixture), "acceptedaccepted");
    let ExecutionPortMessage::ModelChunkMessage(mut changed) = accepted else {
        panic!("chunk");
    };
    let replacement = br#"{"delta":"changed","type":"output_text_delta"}"#;
    changed.payload.as_mut().unwrap().data_base64 = STANDARD.encode(replacement);
    changed.payload.as_mut().unwrap().payload_digest =
        Sha256Digest(format!("sha256:{:x}", Sha256::digest(replacement)));
    assert!(
        fixture
            .accept(&ExecutionPortMessage::ModelChunkMessage(changed), expired())
            .is_err()
    );
    assert_eq!(assistant_content(&mut fixture), "acceptedaccepted");
    fixture.close();
}

#[test]
fn renewed_chat_lease_accepts_new_frame_with_original_dispatch_stamp() {
    let (mut fixture, first) = prepared_provider_fixture(96);
    let lease = &first.lease;
    fixture
        .storage
        .execution_registry()
        .unwrap()
        .renew_execution_lease(&winwincode_storage::ExecutionLeaseRenewal {
            expires_at: at(58),
            prior_expires_at: lease.expires_at.clone(),
            job_id: lease.job_id.clone(),
            lease_id: lease.lease_id.clone(),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
            attempt: 1,
            fencing_token: lease.fencing_token.clone(),
            message_id: ExecutionMessageId(id("xmsg", 96961)),
            request_id: RequestId(id("req", 96961)),
            sent_at: at(15),
        })
        .unwrap();
    let ExecutionPortMessage::ModelChunkMessage(mut chunk) = model_binding_message(first) else {
        panic!("chunk");
    };
    chunk.sequence = ExecutionSequence(2);
    chunk.message_id = ExecutionMessageId(id("xmsg", 96962));
    let frame = ExecutionPortMessage::ModelChunkMessage(chunk.clone());
    assert!(chunk.lease.expires_at.0 < at(55).0);
    fixture
        .accept(&frame, at(55))
        .expect("trusted renewal extends original proven period");
    fixture = fixture.restart();
    fixture
        .accept(&frame, expired())
        .expect("expired exact frame replay after renewal");
    chunk.sequence = ExecutionSequence(3);
    chunk.message_id = ExecutionMessageId(id("xmsg", 96963));
    fixture
        .accept(&ExecutionPortMessage::ModelChunkMessage(chunk), at(59))
        .expect("retained output remains admissible after the renewed deadline");
    fixture.close();
}

#[test]
fn worker_message_numbers_do_not_conflict_between_chat_jobs() {
    let mut fixture = Fixture::open(950);
    let mut messages = Vec::new();
    for seed in [951, 952] {
        fixture.product_session_id = ProductSessionId(id("psn", seed));
        let job = create_chat_job(&mut fixture, seed);
        let scope = execution_scope(&fixture);
        register_worker(&mut fixture.storage, seed);
        reserve_execution(&mut fixture.storage, &scope, &job, seed);
        transition_job(
            &mut fixture.storage,
            &scope,
            &job.job_id,
            seed * 100 + 20,
            1,
            ExecutionJobState::Queued,
            ExecutionJobState::Leased,
        );
        let runtime = claim_and_open_runtime(&mut fixture, &job, seed);
        accept_dispatch(&mut fixture, &job, &runtime, seed);
        let mut binding = binding_message(&fixture, &runtime, seed);
        binding.message_id = ExecutionMessageId(id("xmsg", 3));
        let message = ExecutionPortMessage::SessionBindingMessage(binding);
        fixture
            .accept(&message, at(9))
            .expect("independent Worker binding");
        messages.push(message);
    }
    let mut fixture = fixture.restart();
    for message in messages {
        fixture
            .accept(&message, at(9))
            .expect("exact binding replay after restart");
    }
    fixture.close();
}

fn chat_project_artifact_messages(
    fixture: &Fixture,
    runtime: &RuntimeAuthority,
) -> (ExecutionPortMessage, ExecutionPortMessage) {
    let ExecutionPortMessage::ArtifactOpenMessage(mut open) = execution_message("artifact.open")
    else {
        panic!("artifact open")
    };
    open.lease = runtime.lease.clone();
    open.worker_session_id = runtime.slot.worker_session_id.clone();
    open.session_identity = session_identity(fixture, runtime);
    open.sent_at = at(11);
    let ExecutionPortMessage::ArtifactChunkMessage(mut chunk) = execution_message("artifact.chunk")
    else {
        panic!("artifact chunk")
    };
    chunk.lease = runtime.lease.clone();
    chunk.worker_session_id = open.worker_session_id.clone();
    chunk.session_identity = open.session_identity.clone();
    chunk.sent_at = at(12);
    open.artifact.digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"{}")));
    open.artifact.media_type = "application/zip".into();
    open.artifact.kind = winwincode_execution_port::generated::ArtifactKind::Candidate;
    open.artifact.file_name = Some("project.zip".into());
    chunk
        .payload
        .content_type
        .clone_from(&open.artifact.media_type);
    chunk.payload.payload_digest = open.artifact.digest.clone();
    (
        ExecutionPortMessage::ArtifactOpenMessage(open),
        ExecutionPortMessage::ArtifactChunkMessage(chunk),
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn streamed_chat_completes_only_through_durable_outcome_and_retains_follow_up_context() {
    let BoundSchedulerFixture {
        mut fixture,
        runtime,
        ..
    } = bound_scheduler_fixture(89);
    let running = fixture.record();
    let binding = &running.bindings()[0];
    let starting_revision = running.session().revision();
    let append = |request, sequence, text: &str| AppendAssistantMessageCommand {
        context: command_context(&fixture.repository_scope, request, 2),
        product_session_id: fixture.product_session_id.clone(),
        binding_identity: binding.binding().identity().clone(),
        runtime_authority: runtime.slot.clone(),
        execution_scope: binding.execution_scope().clone(),
        worker_pool_id: binding.worker_pool_id().clone(),
        model_exchange_id: binding.model_exchange_id().clone(),
        stream_sequence: sequence,
        public_text_delta: text.into(),
        state: AssistantMessageState::Streaming,
        terminal_outcome: None,
    };
    let first = append(89_020, 1, "Hello, ");
    let second = append(89_021, 2, "world!");
    ProductSessionService::new(&mut fixture.storage)
        .append_assistant_message(&first)
        .expect("first public delta");
    ProductSessionService::new(&mut fixture.storage)
        .update_metadata(
            &command_context(&fixture.repository_scope, 89_022, starting_revision),
            &fixture.product_session_id,
            "Archived while streaming",
            true,
        )
        .expect("metadata changes retain the original execution revision");
    ProductSessionService::new(&mut fixture.storage)
        .append_assistant_message(&second)
        .expect("second public delta");
    let outcome =
        ExecutionPortMessage::JobOutcomeMessage(successful_outcome(&fixture, &runtime, 89));
    let mut acknowledged = fixture
        .accept(&outcome, at(20))
        .expect("canonical Worker terminal outcome");
    let completed = fixture.record();
    assert_eq!(
        completed.session().state(),
        winwincode_control_plane::ProductSessionState::Idle
    );
    assert_eq!(completed.session().revision(), starting_revision + 2);
    assert_eq!(
        completed.turn_intents()[0].state,
        winwincode_control_plane::ProductSessionTurnState::Completed
    );
    let assistant = completed
        .messages()
        .iter()
        .find(|message| message.role == "assistant")
        .unwrap();
    assert_eq!(assistant.content, "Hello, world!");
    assert_eq!(assistant.state, "completed");
    let mut fixture = fixture.restart();
    let ExecutionPortMessage::JobOutcomeAckMessage(original_ack) = &mut acknowledged[0] else {
        panic!("JobOutcome acknowledgement")
    };
    assert_eq!(original_ack.status, JobOutcomeAckMessageStatus::Accepted);
    original_ack.status = JobOutcomeAckMessageStatus::Duplicate;
    assert_eq!(
        fixture
            .accept(&outcome, expired())
            .expect("original receipt replays after restart"),
        acknowledged
    );
    assert_eq!(fixture.record(), completed);
    let execution_config = ProductSessionExecutionConfig::try_new(
        fixture.repository_scope.clone(),
        "0123456789abcdef0123456789abcdef01234567",
        "codex-chat",
        Some(3_600),
        1_073_741_824,
    )
    .unwrap();
    let follow_up = ProductSessionService::new(&mut fixture.storage)
        .submit_chat(&SubmitChatMessageCommand {
            context: command_context(
                &fixture.repository_scope,
                89_030,
                completed.session().revision(),
            ),
            product_session_id: fixture.product_session_id.clone(),
            message: "Repeat the greeting".into(),
            attachments: Vec::new(),
            execution_config,
        })
        .expect("next turn after terminal receipt recovery");
    let follow_up_scope = execution_scope(&fixture);
    let queued = fixture
        .storage
        .execution_queue()
        .unwrap()
        .load_job(&follow_up_scope, &follow_up.turn_intent.execution_job_id)
        .unwrap()
        .unwrap();
    let job: ExecutionJob = serde_json::from_slice(&queued.dispatch_payload).unwrap();
    assert!(job.goal.contains("Hello, world!"));
    assert!(job.goal.contains("run the canonical Chat turn"));
    assert!(
        job.goal
            .ends_with("Current user message:\nRepeat the greeting")
    );
    assert!(job.goal.len() <= 20_000);
    fixture.close();
}

#[test]
#[allow(clippy::too_many_lines)]
fn chat_artifact_ingress_preserves_exact_session_binding_and_replays() {
    let BoundSchedulerFixture {
        mut fixture,
        runtime,
        ..
    } = bound_scheduler_fixture(85);
    let (open, chunk) = chat_project_artifact_messages(&fixture, &runtime);
    let ExecutionPortMessage::ArtifactOpenMessage(project_open) = &open else {
        panic!("project artifact open")
    };
    let artifact = winwincode_execution_port::generated::ArtifactReference {
        artifact_id: project_open.artifact.artifact_id.clone(),
        digest: project_open.artifact.digest.clone(),
    };
    for message in [&open, &chunk] {
        let replies = fixture
            .accept(message, at(13))
            .expect("Chat artifact is acknowledged");
        assert!(replies.iter().any(|reply| matches!(reply,
            ExecutionPortMessage::ArtifactAckMessage(ack) if ack.status == LeaseWriteStatus::Accepted)));
    }
    let preparing = fixture.record();
    let preparing_message = preparing
        .messages()
        .iter()
        .find(|message| message.role == "assistant")
        .expect("assistant output preparation");
    assert_eq!(preparing_message.state, "streaming");
    assert_eq!(
        preparing_message.output_preparation,
        Some("preparing".to_owned())
    );
    assert!(
        preparing_message.artifact_refs.is_none(),
        "an unfinished project is not downloadable"
    );
    let mut fixture = fixture.restart();
    assert_eq!(fixture.record().messages(), preparing.messages());
    for message in [&open, &chunk] {
        let replies = fixture.accept(message, at(14)).expect("artifact replay");
        assert!(replies.iter().any(|reply| matches!(reply,
            ExecutionPortMessage::ArtifactAckMessage(ack) if matches!(ack.status, LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate))));
    }
    let mut outcome = successful_outcome(&fixture, &runtime, 85);
    outcome.outcome.artifacts = vec![artifact.clone()];
    let mut injected = outcome.clone();
    injected.outcome.artifacts[0].digest = Sha256Digest(format!("sha256:{}", "0".repeat(64)));
    assert!(
        fixture
            .accept(&ExecutionPortMessage::JobOutcomeMessage(injected), at(20))
            .is_err()
    );
    fixture
        .accept(&ExecutionPortMessage::JobOutcomeMessage(outcome), at(20))
        .expect("Chat output links are durable");
    let completed = fixture.record();
    let message = completed
        .messages()
        .iter()
        .find(|message| message.role == "assistant")
        .unwrap();
    assert_eq!(message.state, "completed");
    assert!(message.output_preparation.is_none());
    assert_eq!(
        message.artifact_refs.as_ref().unwrap()[0].artifact_id,
        artifact.artifact_id
    );
    let mut fixture = fixture.restart();
    for frame in [&open, &chunk] {
        fixture
            .accept(frame, expired())
            .expect("exact artifact replay after terminal");
    }
    assert_eq!(
        fixture.record().messages(),
        completed.messages(),
        "late exact replay must not restore preparation"
    );
    let mut query: winwincode_api::generated::SessionArtifactGetQuery = from_value(serde_json::json!({
        "actor":{"id":id("usr",1),"kind":"user"}, "query":"session.artifact.get",
        "parameters":{"productSessionId":fixture.product_session_id,"artifactId":artifact.artifact_id,"offset":0,"length":262_144},
        "page":{"cursor":null,"limit":1}, "requestId":id("req",85999),"schemaVersion":"winwincode/v1","scope":fixture.repository_scope,
    })).expect("download query");
    let downloaded = fixture
        .control_plane
        .session_artifact_get(&mut fixture.storage, &query)
        .expect("download after restart and terminal cleanup");
    assert_eq!(
        STANDARD
            .decode(downloaded.result.data_base64)
            .expect("base64"),
        b"{}"
    );
    assert_eq!(downloaded.result.digest, artifact.digest);
    query.parameters.product_session_id = ProductSessionId(id("psn", 85999));
    assert!(
        fixture
            .control_plane
            .session_artifact_get(&mut fixture.storage, &query)
            .is_err()
    );
    fixture.close();
}

#[test]
fn accepted_chat_artifact_ack_survives_projection_failure_and_cancel_race() {
    for cancel_before_recovery in [false, true] {
        let BoundSchedulerFixture {
            mut fixture,
            runtime,
            ..
        } = bound_scheduler_fixture(87);
        let (open, _) = chat_project_artifact_messages(&fixture, &runtime);
        let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_chat_output_projection BEFORE UPDATE ON product_state
             WHEN NEW.stream_id LIKE 'product-sessions:%'
             BEGIN SELECT RAISE(ABORT, 'injected projection failure'); END;",
            )
            .unwrap();
        let replies = fixture
            .accept(&open, at(13))
            .expect("canonical Artifact ACK survives auxiliary projection failure");
        assert!(replies.iter().any(|reply| matches!(reply,
            ExecutionPortMessage::ArtifactAckMessage(ack) if ack.status == LeaseWriteStatus::Accepted)));
        assert!(
            fixture
                .record()
                .messages()
                .iter()
                .all(|message| message.output_preparation.is_none())
        );
        connection
            .execute_batch("DROP TRIGGER fail_chat_output_projection")
            .unwrap();
        drop(connection);
        if cancel_before_recovery {
            let current = fixture.record();
            let mut context = command_context(
                &fixture.repository_scope,
                87_020,
                current.session().revision(),
            );
            context.occurred_at = at(14);
            ProductSessionService::new(&mut fixture.storage)
                .cancel_session(&CancelProductSessionCommand {
                    context,
                    product_session_id: fixture.product_session_id.clone(),
                    actor: winwincode_session::AuthenticatedActor::User(UserId(id("usr", 1))),
                    reason: "cancel before auxiliary projection recovery".into(),
                })
                .expect("cancel while artifact projection is missing");
        }
        let mut fixture = fixture.restart();
        let replies = fixture
            .accept(&open, at(15))
            .expect("exact accepted Artifact retains its ACK on recovery");
        assert!(replies.iter().any(|reply| matches!(reply,
            ExecutionPortMessage::ArtifactAckMessage(ack) if ack.status == LeaseWriteStatus::Duplicate)));
        let current = fixture.record();
        if cancel_before_recovery {
            assert_eq!(
                current.session().state(),
                winwincode_control_plane::ProductSessionState::Cancelled
            );
            assert!(
                current
                    .messages()
                    .iter()
                    .all(|message| message.output_preparation.is_none())
            );
        } else {
            assert!(
                current
                    .messages()
                    .iter()
                    .any(|message| message.output_preparation.as_deref() == Some("preparing"))
            );
        }
        fixture.close();
    }
}

#[test]
fn only_canonical_chat_project_artifact_derives_output_preparation() {
    use winwincode_execution_port::generated::ArtifactKind;
    for (kind, media_type, file_name) in [
        (ArtifactKind::Diff, "application/zip", "project.zip"),
        (
            ArtifactKind::Candidate,
            "application/octet-stream",
            "project.zip",
        ),
        (ArtifactKind::Candidate, "application/zip", "other.zip"),
    ] {
        let BoundSchedulerFixture {
            mut fixture,
            runtime,
            ..
        } = bound_scheduler_fixture(88);
        let (ExecutionPortMessage::ArtifactOpenMessage(mut open), _) =
            chat_project_artifact_messages(&fixture, &runtime)
        else {
            panic!("project artifact open")
        };
        open.artifact.kind = kind;
        open.artifact.media_type = media_type.into();
        open.artifact.file_name = Some(file_name.into());
        fixture
            .accept(&ExecutionPortMessage::ArtifactOpenMessage(open), at(13))
            .expect("non-project Artifact accepted");
        assert!(
            fixture
                .record()
                .messages()
                .iter()
                .all(|message| message.output_preparation.is_none())
        );
        fixture.close();
    }
}

#[test]
fn chat_output_preparation_cancellation_survives_restart_and_exact_artifact_replay() {
    let BoundSchedulerFixture {
        mut fixture,
        runtime,
        ..
    } = bound_scheduler_fixture(86);
    let (open, _) = chat_project_artifact_messages(&fixture, &runtime);
    fixture.accept(&open, at(13)).expect("prepare Chat output");
    let preparing = fixture.record();
    assert!(
        preparing
            .messages()
            .iter()
            .any(|message| message.output_preparation.as_deref() == Some("preparing"))
    );
    let mut context = command_context(
        &fixture.repository_scope,
        86_020,
        preparing.session().revision(),
    );
    context.occurred_at = at(14);
    let cancel = CancelProductSessionCommand {
        context,
        product_session_id: fixture.product_session_id.clone(),
        actor: winwincode_session::AuthenticatedActor::User(UserId(id("usr", 1))),
        reason: "cancel output preparation".into(),
    };
    ProductSessionService::new(&mut fixture.storage)
        .cancel_session(&cancel)
        .expect("cancel prepared output");
    let cancelled = fixture.record();
    let assistant = cancelled
        .messages()
        .iter()
        .find(|message| message.role == "assistant")
        .expect("cancelled assistant");
    assert_eq!(assistant.state, "cancelled");
    assert!(assistant.output_preparation.is_none());
    assert!(assistant.artifact_refs.is_none());
    let mut fixture = fixture.restart();
    ProductSessionService::new(&mut fixture.storage)
        .cancel_session(&cancel)
        .expect("exact cancellation replay");
    fixture
        .accept(&open, at(15))
        .expect("exact artifact replay after cancel");
    assert_eq!(fixture.record().messages(), cancelled.messages());
    fixture.close();
}

#[test]
#[allow(clippy::too_many_lines)]
fn core_chat_approval_is_recorded_for_the_owner_and_survives_replay() {
    for renewed in [false, true] {
        let BoundSchedulerFixture {
            mut fixture,
            runtime,
            ..
        } = bound_scheduler_fixture(84);
        let ExecutionPortMessage::ApprovalRequestMessage(mut request) =
            execution_message("approval.request")
        else {
            panic!("approval fixture")
        };
        request.lease = runtime.lease.clone();
        request.worker_session_id = runtime.slot.worker_session_id.clone();
        request.session_identity = session_identity(&fixture, &runtime);
        request.sent_at = at(11);
        request.expires_at = at(24);
        if renewed {
            let lease = &request.lease;
            assert_eq!(
                fixture
                    .storage
                    .execution_registry()
                    .expect("registry")
                    .renew_execution_lease(&winwincode_storage::ExecutionLeaseRenewal {
                        expires_at: at(58),
                        prior_expires_at: lease.expires_at.clone(),
                        job_id: lease.job_id.clone(),
                        lease_id: lease.lease_id.clone(),
                        worker_id: lease.worker_id.clone(),
                        worker_instance_id: lease.worker_instance_id.clone(),
                        attempt: 1,
                        fencing_token: lease.fencing_token.clone(),
                        message_id: ExecutionMessageId(id("xmsg", 84997)),
                        request_id: RequestId(id("req", 84997)),
                        sent_at: at(10),
                    })
                    .expect("renew before original approval arrives")
                    .status,
                winwincode_storage::LeaseWriteStatus::Accepted
            );
        }
        let message = ExecutionPortMessage::ApprovalRequestMessage(request.clone());
        fixture
            .accept(&message, at(12))
            .expect("Core approval must reach Chat without a preseeded Gate fact");
        let mut fixture = fixture.restart();
        fixture
            .accept(&message, at(13))
            .expect("exact approval replay");
        let query = from_value(serde_json::json!({
            "actor": {"id": id("usr",1), "kind":"user"},
            "page": {"cursor":null,"limit":1},
            "parameters":{"approvalId":request.approval_id},
            "query":"approval.get", "requestId":id("req",84999),
            "schemaVersion":"winwincode/v1", "scope":fixture.repository_scope,
        }))
        .expect("approval query");
        let response = winwincode_control_plane::ChatInteractionService::new(&mut fixture.storage)
            .approval_get(&fixture.receipt_scope, &query, &at(14))
            .expect("visible approval");
        assert_eq!(response.result.state, "pending");
        assert!(response.result.decision_enabled);
        let command: winwincode_api::generated::ApprovalDecideCommand =
            from_value(serde_json::json!({
                "actor": {"id": id("usr",1), "kind":"user"},
                "command":"approval.decide", "expectedRevision":response.result.revision,
                "payload":{"approvalId":request.approval_id,"binding":response.result.binding,
                    "decision":"approve","reason":"Install project dependencies"},
                "requestId":id("req",84998),"schemaVersion":"winwincode/v1",
                "scope":fixture.repository_scope,
            }))
            .expect("approval decision");
        let mut context = command_context(&fixture.repository_scope, 84998, 1);
        context.occurred_at = at(15);
        let mut foreign = command.clone();
        foreign.actor =
            from_value(serde_json::json!({"id":id("usr",2),"kind":"user"})).expect("actor");
        assert!(
            winwincode_control_plane::ChatInteractionService::new(&mut fixture.storage)
                .decide_approval(&context, &foreign)
                .is_err()
        );
        let mut expired_context = context.clone();
        expired_context.occurred_at = at(25);
        assert!(
            winwincode_control_plane::ChatInteractionService::new(&mut fixture.storage)
                .decide_approval(&expired_context, &command)
                .is_err()
        );
        let receipt = winwincode_control_plane::ChatInteractionService::new(&mut fixture.storage)
            .decide_approval(&context, &command)
            .expect("owner approval");
        assert_eq!(receipt.approval.state, "approved");
        let decision = receipt.worker_decision.expect("Worker decision");
        assert_eq!(decision.lease, runtime.lease);
        assert_eq!(decision.session_identity, request.session_identity);
        let mut fixture = fixture.restart();
        let replay = winwincode_control_plane::ChatInteractionService::new(&mut fixture.storage)
            .decide_approval(&context, &command)
            .expect("decision replay");
        assert!(replay.replayed);
        assert_eq!(replay.worker_decision, Some(decision));
        fixture.close();
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn chat_action_receipt_uses_chat_authority_and_rejects_foreign_or_expired_requests() {
    use winwincode_execution_port::action_enforcement::{
        ActionEnforcementIssuer, ActionEnforcementSigningKey,
    };
    let BoundSchedulerFixture {
        mut fixture,
        job,
        runtime,
        ..
    } = bound_scheduler_fixture(81);
    let ExecutionPortMessage::ActionEnforcementRequestMessage(mut request) =
        execution_message("action.enforcement_request")
    else {
        panic!("action request fixture")
    };
    request.job_id = job.job_id.clone();
    request.lease = runtime.lease.clone();
    request.worker_session_id = runtime.slot.worker_session_id.clone();
    request.session_identity = session_identity(&fixture, &runtime);
    request.sent_at = at(11);
    assert!(runtime.lease.expires_at.0 < at(55).0);
    let lease = &request.lease;
    assert_eq!(
        fixture
            .storage
            .execution_registry()
            .expect("registry")
            .renew_execution_lease(&winwincode_storage::ExecutionLeaseRenewal {
                expires_at: at(58),
                prior_expires_at: lease.expires_at.clone(),
                job_id: lease.job_id.clone(),
                lease_id: lease.lease_id.clone(),
                worker_id: lease.worker_id.clone(),
                worker_instance_id: lease.worker_instance_id.clone(),
                attempt: 1,
                fencing_token: lease.fencing_token.clone(),
                message_id: ExecutionMessageId(id("xmsg", 81997)),
                request_id: RequestId(id("req", 81997)),
                sent_at: at(10),
            })
            .expect("renew before delayed action arrives")
            .status,
        winwincode_storage::LeaseWriteStatus::Accepted
    );
    let issuer = ActionEnforcementIssuer::new(
        ActionEnforcementSigningKey::from_bytes([11; 32]).expect("key"),
    );
    let first = winwincode_control_plane::issue_action_enforcement_receipt(
        &mut fixture.storage,
        &issuer,
        &at(55),
        &request,
    )
    .expect("Chat tool must receive its own signed action receipt");
    issuer.verify_signature(&first).expect("valid signature");
    assert_eq!(first.actor.id, UserId(id("usr", 1)));
    assert_eq!(first.scope, fixture.repository_scope);
    assert_eq!(first.lease, request.lease);
    assert!(first.evaluated_at.0 > request.lease.expires_at.0);
    for mutate in 0..3 {
        let mut changed = request.clone();
        match mutate {
            0 => changed.session_identity.product_session_id = ProductSessionId(id("psn", 999)),
            1 => changed.lease.worker_instance_id = WorkerInstanceId(id("wki", 999)),
            _ => {
                changed.session_identity.codex_thread_id =
                    CodexThreadId("foreign-thread".to_owned());
            }
        }
        assert!(
            winwincode_control_plane::issue_action_enforcement_receipt(
                &mut fixture.storage,
                &issuer,
                &at(56),
                &changed
            )
            .is_err()
        );
    }
    assert!(
        winwincode_control_plane::issue_action_enforcement_receipt(
            &mut fixture.storage,
            &issuer,
            &expired(),
            &request
        )
        .is_err()
    );
    let mut fixture = fixture.restart();
    let replay = winwincode_control_plane::issue_action_enforcement_receipt(
        &mut fixture.storage,
        &issuer,
        &at(56),
        &request,
    )
    .expect("replay after restart");
    assert_eq!(first, replay);
    let scope = execution_scope(&fixture);
    let revision = fixture
        .storage
        .load_execution_job_record(&job.job_id)
        .expect("job read")
        .expect("job")
        .revision;
    transition_job(
        &mut fixture.storage,
        &scope,
        &job.job_id,
        81999,
        revision,
        ExecutionJobState::Running,
        ExecutionJobState::Failed,
    );
    assert!(
        winwincode_control_plane::issue_action_enforcement_receipt(
            &mut fixture.storage,
            &issuer,
            &at(57),
            &request
        )
        .is_err(),
        "terminal Chat cannot authorize another action"
    );
    fixture.close();
}

#[test]
fn successful_chat_with_unknown_usage_completes_and_replays_after_restart() {
    let seed = 91;
    let mut fixture = Fixture::open(seed);
    let job = create_chat_job(&mut fixture, seed);
    let scope = execution_scope(&fixture);
    register_worker(&mut fixture.storage, seed);
    reserve_execution(&mut fixture.storage, &scope, &job, seed);
    transition_job(
        &mut fixture.storage,
        &scope,
        &job.job_id,
        seed * 100 + 20,
        1,
        ExecutionJobState::Queued,
        ExecutionJobState::Leased,
    );
    let runtime = claim_and_open_runtime(&mut fixture, &job, seed);
    accept_dispatch(&mut fixture, &job, &runtime, seed);
    let binding = binding_message(&fixture, &runtime, seed);
    fixture
        .accept(&ExecutionPortMessage::SessionBindingMessage(binding), at(9))
        .unwrap();
    let model_open = model_open_message(&fixture, &runtime, seed);
    fixture
        .accept(&model_binding_message(model_open), at(10))
        .unwrap();
    let mut outcome = successful_outcome(&fixture, &runtime, seed);
    let usage = winwincode_execution_port::generated::ExecutionOutcomeUsage::unknown(1000, 48);
    outcome.outcome.usage = Some(usage.clone());
    let message = ExecutionPortMessage::JobOutcomeMessage(outcome);
    assert_eq!(
        outcome_status(&fixture.accept(&message, at(20)).unwrap()),
        JobOutcomeAckMessageStatus::Accepted
    );
    fixture = fixture.restart();
    assert_eq!(
        outcome_status(&fixture.accept(&message, expired()).unwrap()),
        JobOutcomeAckMessageStatus::Duplicate
    );
    let record = ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        record
            .messages()
            .iter()
            .find(|message| message.role == "assistant")
            .unwrap()
            .state,
        "completed"
    );
    let terminal = record.turn_intents()[0].terminal_outcome.as_ref().unwrap();
    assert_eq!(terminal.status, ExecutionOutcomeStatus::Succeeded);
    assert_eq!(terminal.usage, Some(usage));
    let reservation = fixture
        .storage
        .execution_admission()
        .unwrap()
        .load_reservation_by_job(&job.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, ExecutionReservationState::Released);
    assert_eq!(reservation.actual_tokens, None);
    assert_eq!(reservation.actual_cost_microunits, None);
    assert_slot_state(&mut fixture, &runtime, WorkerSlotState::Completed);
    assert_eq!(
        fixture
            .storage
            .execution_queue()
            .unwrap()
            .load_job(&scope, &job.job_id)
            .unwrap()
            .unwrap()
            .state,
        ExecutionJobState::Completed
    );
    fixture.close();
}

#[test]
fn failure_with_known_or_unknown_usage_replays_after_metadata_change() {
    for known_usage in [false, true] {
        let seed = 90;
        let mut fixture = Fixture::open(seed);
        let job = create_chat_job(&mut fixture, seed);
        let scope = execution_scope(&fixture);
        register_worker(&mut fixture.storage, seed);
        reserve_execution(&mut fixture.storage, &scope, &job, seed);
        transition_job(
            &mut fixture.storage,
            &scope,
            &job.job_id,
            seed * 100 + 20,
            1,
            ExecutionJobState::Queued,
            ExecutionJobState::Leased,
        );
        let runtime = claim_and_open_runtime(&mut fixture, &job, seed);
        accept_dispatch(&mut fixture, &job, &runtime, seed);
        let binding = binding_message(&fixture, &runtime, seed);
        fixture
            .accept(&ExecutionPortMessage::SessionBindingMessage(binding), at(9))
            .unwrap();
        ProductSessionService::new(&mut fixture.storage)
            .update_metadata(
                &command_context(&fixture.repository_scope, 9099, 2),
                &fixture.product_session_id,
                "Renamed before failure",
                true,
            )
            .unwrap();
        let mut outcome = successful_outcome(&fixture, &runtime, seed);
        outcome.outcome.status = ExecutionOutcomeStatus::Failed;
        if !known_usage {
            outcome.outcome.usage = None;
        }
        let expected_usage = outcome.outcome.usage.clone();
        let message = ExecutionPortMessage::JobOutcomeMessage(outcome);
        let accepted = fixture
            .accept(&message, at(20))
            .expect("early failure accepted");
        assert_eq!(
            outcome_status(&accepted),
            JobOutcomeAckMessageStatus::Accepted
        );
        let record = ProductSessionService::new(&mut fixture.storage)
            .get(&fixture.receipt_scope, &fixture.product_session_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            record.session().state(),
            winwincode_session::ProductSessionState::Failed
        );
        assert!(record.bindings().is_empty());
        assert!(record.turn_intents()[0].terminal_outcome.is_some());
        assert_eq!(record.messages().len(), 2);
        let assistant = record
            .messages()
            .iter()
            .find(|message| message.role == "assistant")
            .unwrap();
        assert_eq!(assistant.state, "failed");
        assert!(assistant.content.is_empty());
        let mut fixture = fixture.restart();
        assert_eq!(
            outcome_status(&fixture.accept(&message, at(700)).unwrap()),
            JobOutcomeAckMessageStatus::Duplicate
        );
        let reservation = fixture
            .storage
            .execution_admission()
            .unwrap()
            .load_reservation_by_job(&job.job_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            reservation.state,
            if known_usage {
                ExecutionReservationState::Settled
            } else {
                ExecutionReservationState::Released
            }
        );
        assert_eq!(
            reservation.actual_tokens,
            expected_usage
                .and_then(|usage| usage.tokens.map(|tokens| u64::try_from(tokens).unwrap()))
        );
        fixture.close();
    }
}

fn register_authenticated_terminal_worker(
    fixture: &mut Fixture,
    worker_seed: u64,
    instance_seed: u64,
    request_seed: u64,
    second: u64,
) {
    let identity = WorkerAuthenticationIdentity::TransportPrincipal {
        issuer: "fixture-authenticated-worker".into(),
        subject: "accounting-replay-worker".into(),
        credential_fingerprint: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
    };
    let management_scope = WorkerRegistryScope::Repository {
        organization_id: fixture.repository_scope.organization_id.clone(),
        workspace_id: fixture.repository_scope.workspace_id.clone(),
        project_id: fixture.repository_scope.project_id.clone(),
        repository_id: fixture.repository_scope.repository_id.clone(),
    };
    let mut registry = fixture.storage.execution_registry().unwrap();
    registry
        .register_worker_for_scope(
            &WorkerRegistrationRequest {
                authentication_identity: identity.clone(),
                protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
                platform: WorkerPlatform::Aarch64AppleDarwin,
                capabilities: vec!["codex".into()],
                capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
                security_zone: "local".into(),
                max_slots: 1,
                message_id: ExecutionMessageId(id("xmsg", request_seed)),
                request_id: RequestId(id("req", request_seed)),
                sent_at: at(second),
                started_at: at(second),
                worker_id: WorkerId(id("wrk", worker_seed)),
                worker_instance_id: WorkerInstanceId(id("wki", instance_seed)),
            },
            &management_scope,
        )
        .unwrap();
    registry
        .record_authenticated_worker_placement(&AuthenticatedWorkerPlacement {
            worker_id: WorkerId(id("wrk", worker_seed)),
            worker_instance_id: WorkerInstanceId(id("wki", instance_seed)),
            worker_pool_id: worker_pool(worker_seed),
            management_scope,
            authentication_identity: identity,
            registration_request_id: RequestId(id("req", request_seed)),
            placed_at: at(second),
        })
        .unwrap();
    registry
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 1,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 1,
            running_slots: 0,
            message_id: ExecutionMessageId(id("xmsg", request_seed + 1)),
            observed_at: at(second + 1),
            sent_at: at(second + 1),
            worker_id: WorkerId(id("wrk", worker_seed)),
            worker_instance_id: WorkerInstanceId(id("wki", instance_seed)),
        })
        .unwrap();
}

#[test]
fn authenticated_predecessor_accounting_completion_preserves_rev1_cancelled_cleanup_replay() {
    authenticated_predecessor_cancelled_cleanup_replay(true);
    authenticated_predecessor_cancelled_cleanup_replay(false);
}

#[allow(
    clippy::too_many_lines,
    reason = "one authenticated two-attempt fixture freezes cancellation, fails cleanup, imports a signed predecessor receipt and replays the original raw outcome"
)]
fn authenticated_predecessor_cancelled_cleanup_replay(control_plane_cancelled: bool) {
    use winwincode_domain::ModelExchangeId;
    use winwincode_execution_port::{
        accounting::{AttemptAccountingStatement, ProviderAccountingReceipt},
        action_enforcement::ActionEnforcementSigningKey,
        generated::ExecutionOutcomeUsage,
    };

    let seed = 94;
    let mut fixture = Fixture::open(seed);
    let job = create_chat_job(&mut fixture, seed);
    let scope = execution_scope(&fixture);
    reserve_execution(&mut fixture.storage, &scope, &job, seed);
    let register_authenticated = |fixture: &mut Fixture, instance_seed, request_seed, second| {
        register_authenticated_terminal_worker(fixture, seed, instance_seed, request_seed, second);
    };
    register_authenticated(&mut fixture, seed, 94_101, 1);
    let original = scheduler_claim(&mut fixture, seed, seed, 94_103, "auth-accounting-old", 5);
    let original_session = WorkerSessionId(id("wsn", 94_104));
    accept_scheduler_dispatch(&mut fixture, &original, &original_session, 94_105, 7);
    let original_runtime =
        open_scheduler_runtime(&mut fixture, &original, 94_104, 94_106, 94_107, 8);
    let binding = binding_message(&fixture, &original_runtime, 94_108);
    fixture
        .accept(&ExecutionPortMessage::SessionBindingMessage(binding), at(9))
        .unwrap();
    let open = model_open_message(&fixture, &original_runtime, 94_109);
    fixture
        .accept(&model_binding_message(open), at(10))
        .unwrap();

    let original_lease = fixture
        .storage
        .execution_registry()
        .unwrap()
        .load_lease(&job.job_id)
        .unwrap()
        .unwrap();
    register_authenticated(&mut fixture, seed + 1, 94_110, 11);
    let replacement = scheduler_claim(
        &mut fixture,
        seed,
        seed + 1,
        94_112,
        "auth-accounting-new",
        25,
    );
    assert_eq!(replacement.job.attempt, 2);
    assert!(replacement.replacement_authority.is_some());
    // This focused regression starts at the CP cleanup boundary. Device
    // recovery owns creation of the financially unresolved predecessor; seed
    // that ledger precondition with the exact real expired lease. The signed
    // Provider importer and durable outcome ingress remain production code.
    let accounting_fixture = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    accounting_fixture
        .execute(
            "INSERT INTO execution_attempt_accounting_pending(job_id,attempt,lease_json,observed_at) VALUES(?1,?2,?3,?4)",
            rusqlite::params![
                job.job_id.0,
                i64::try_from(original_lease.attempt).unwrap(),
                serde_json::to_string(&original_lease).unwrap(),
                at(25).0
            ],
        )
        .unwrap();
    drop(accounting_fixture);
    let replacement_session = WorkerSessionId(id("wsn", 94_113));
    accept_scheduler_dispatch(&mut fixture, &replacement, &replacement_session, 94_114, 26);
    let runtime = open_scheduler_runtime(&mut fixture, &replacement, 94_113, 94_115, 94_116, 26);
    let mut binding = binding_message(&fixture, &runtime, 94_117);
    binding.bound_at = at(27);
    binding.sent_at = at(27);
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(binding),
            at(27),
        )
        .unwrap();
    let mut open = model_open_message(&fixture, &runtime, 94_118);
    open.sent_at = at(28);
    fixture
        .accept(&model_binding_message(open), at(28))
        .unwrap();
    assert!(
        fixture
            .storage
            .execution_registry()
            .unwrap()
            .load_lease_placement(&job.job_id)
            .unwrap()
            .is_some()
    );
    assert!(
        fixture
            .storage
            .execution_has_unknown_predecessor(&job.job_id)
            .unwrap()
    );
    if control_plane_cancelled {
        let before = fixture.record();
        let mut context = command_context(
            &fixture.repository_scope,
            94_119,
            before.session().revision(),
        );
        context.occurred_at = at(29);
        ProductSessionService::new(&mut fixture.storage)
            .cancel_session(&CancelProductSessionCommand {
                context,
                product_session_id: fixture.product_session_id.clone(),
                actor: winwincode_session::AuthenticatedActor::User(UserId(id("usr", 1))),
                reason: "cancel authenticated replacement before its raw result arrives".into(),
            })
            .unwrap();
    }
    let before_terminal = fixture
        .storage
        .execution_admission()
        .unwrap()
        .load_reservation_by_job(&job.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        before_terminal.state,
        if control_plane_cancelled {
            ExecutionReservationState::Released
        } else {
            ExecutionReservationState::Running
        }
    );
    let mut outcome = successful_outcome(&fixture, &runtime, 941);
    if !control_plane_cancelled {
        outcome.outcome.status = ExecutionOutcomeStatus::Cancelled;
    }
    outcome.outcome.finished_at = at(30);
    outcome.sent_at = at(30);
    let original_frame = serde_json::to_value(&outcome).unwrap();
    let message = ExecutionPortMessage::JobOutcomeMessage(outcome.clone());
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_authenticated_cancelled_close BEFORE UPDATE ON worker_session_slots WHEN NEW.state='cancelled' BEGIN SELECT RAISE(ABORT, 'injected authenticated terminal cleanup failure'); END;").unwrap();
    assert!(fixture.accept(&message, at(30)).is_err());
    let stream = format!("execution-outcome:{}:2", job.job_id.0);
    let revision_one = fixture.storage.load_state(&stream).unwrap().unwrap();
    assert_eq!(revision_one.revision, 1);
    let frozen_outcome: Value = serde_json::from_slice(&revision_one.payload).unwrap();
    assert_eq!(frozen_outcome["message"], original_frame);
    assert_eq!(frozen_outcome["decision"]["status"], "cancelled");
    assert_eq!(
        frozen_outcome["decision"]["cancellation"].is_object(),
        control_plane_cancelled
    );
    let released = fixture
        .storage
        .execution_admission()
        .unwrap()
        .load_reservation_by_job(&job.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(released.state, ExecutionReservationState::Released);
    if control_plane_cancelled {
        assert_eq!(released, before_terminal);
    }
    let (terminal_id, original_usage_json): (String, String) = connection.query_row("SELECT terminal_request_id,terminal_usage_json FROM execution_admission_reconciliation WHERE job_id=?1", [&job.job_id.0], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    let frozen_usage: ExecutionOutcomeUsage = serde_json::from_str(&original_usage_json).unwrap();
    assert_eq!(frozen_usage.tokens, None);
    assert_eq!(frozen_usage.known_tokens, 23);
    assert_eq!(frozen_usage.runtime_millis, 25_000);

    let key = ActionEnforcementSigningKey::from_bytes([9; 32]).unwrap();
    let exchange = ModelExchangeId(id("mdl", 94_109));
    let slot = format!("primary:{}", exchange.0);
    let mut statement = AttemptAccountingStatement {
        lease: original.lease.clone(),
        manifest: vec![slot.clone()],
        receipts: vec![ProviderAccountingReceipt {
            known_tokens: 0,
            slot,
            model_exchange_id: exchange,
            provider_id: "fixture-auth-provider".into(),
            provider_receipt_id: "predecessor-response-94".into(),
            source_digest: Sha256Digest(format!("sha256:{}", "d".repeat(64))),
            tokens: Some(11),
            cost_microunits: Some(7),
        }],
        signature: Sha256Digest(String::new()),
    };
    statement.sign(&key).unwrap();
    fixture
        .storage
        .reconcile_provider_attempt(&statement.verified(&key).unwrap(), &at(31))
        .unwrap();
    assert!(
        !fixture
            .storage
            .execution_has_unknown_predecessor(&job.job_id)
            .unwrap()
    );
    assert_eq!(
        fixture
            .storage
            .execution_admission()
            .unwrap()
            .load_retained_terminal_usage(&job.job_id, &RequestId(terminal_id.clone()))
            .unwrap(),
        Some(frozen_usage.clone())
    );
    connection
        .execute_batch("DROP TRIGGER fail_authenticated_cancelled_close")
        .unwrap();
    drop(connection);
    fixture = fixture.restart();
    assert_eq!(
        outcome_status(&fixture.accept(&message, expired()).unwrap()),
        JobOutcomeAckMessageStatus::Duplicate
    );
    let ready = fixture.storage.load_state(&stream).unwrap().unwrap();
    assert_eq!(ready.revision, 2);
    let replayed: Value = serde_json::from_slice(&ready.payload).unwrap();
    assert_eq!(replayed["message"], original_frame);
    assert_eq!(replayed["decision"], frozen_outcome["decision"]);
    assert_eq!(
        fixture
            .storage
            .execution_admission()
            .unwrap()
            .load_reservation_by_job(&job.job_id)
            .unwrap()
            .unwrap(),
        released
    );
    assert_slot_state(&mut fixture, &runtime, WorkerSlotState::Cancelled);
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    let (original_usage, final_usage): (String, String) = connection.query_row("SELECT terminal_usage_json,settlement_usage_json FROM execution_admission_reconciliation WHERE job_id=?1", [&job.job_id.0], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(original_usage, original_usage_json);
    let final_usage: ExecutionOutcomeUsage = serde_json::from_str(&final_usage).unwrap();
    assert_eq!(
        (
            final_usage.tokens,
            final_usage.cost_microunits,
            final_usage.runtime_millis
        ),
        (Some(34), Some(36), 25_000)
    );
    assert_eq!(serde_json::to_value(&outcome).unwrap(), original_frame);
    let mut changed = outcome;
    changed.outcome.usage.as_mut().unwrap().cost_microunits = Some(30);
    assert_outcome_conflict_without_write(
        &mut fixture,
        &changed,
        &ExecutionPortErrorCode::MessageConflict,
    );
    assert_eq!(fixture.storage.load_state(&stream).unwrap().unwrap(), ready);
    assert_eq!(
        outcome_status(&fixture.accept(&message, expired()).unwrap()),
        JobOutcomeAckMessageStatus::Duplicate
    );
    drop(connection);
    fixture.close();
}

#[test]
fn authenticated_settled_cleanup_replays_its_frozen_source_after_signed_physical_accounting() {
    for complete in [false, true] {
        settled_cleanup_replay_after_signed_physical_accounting(complete);
    }
}

fn settled_physical_statement(
    lease: &ExecutionLeaseStamp,
    seed: u64,
    complete: bool,
) -> winwincode_execution_port::accounting::AttemptAccountingStatement {
    use winwincode_domain::ModelExchangeId;
    use winwincode_execution_port::accounting::{
        AttemptAccountingStatement, ProviderAccountingReceipt,
    };
    AttemptAccountingStatement {
        lease: lease.clone(),
        manifest: vec!["primary:physical-attempts".into()],
        receipts: vec![ProviderAccountingReceipt {
            slot: "primary:physical-attempts".into(),
            model_exchange_id: ModelExchangeId(id("mdl", seed * 200 + 7)),
            provider_id: "fixture-auth-provider".into(),
            provider_receipt_id: "original-physical-attempt-set".into(),
            source_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            tokens: complete.then_some(16),
            known_tokens: if complete { 16 } else { 5 },
            cost_microunits: complete.then_some(9),
        }],
        signature: Sha256Digest(String::new()),
    }
}

fn settled_cleanup_replay_after_signed_physical_accounting(complete: bool) {
    use winwincode_execution_port::{
        action_enforcement::ActionEnforcementSigningKey,
        generated::{ExecutionOutcomeUsage, ExecutionOutcomeUsageAccountingStatus},
    };
    let seed = 95;
    let mut fixture = Fixture::open(seed);
    let job = create_chat_job(&mut fixture, seed);
    register_authenticated_terminal_worker(&mut fixture, seed, seed, 95_101, 1);
    let scope = execution_scope(&fixture);
    reserve_execution(&mut fixture.storage, &scope, &job, seed);
    let BoundSchedulerFixture {
        mut fixture,
        job,
        runtime,
        ..
    } = finish_bound_scheduler_fixture(fixture, job, seed);
    let mut outcome = successful_outcome(&fixture, &runtime, seed);
    outcome.outcome.usage = Some(ExecutionOutcomeUsage {
        runtime_millis: 12000,
        tokens: Some(5),
        known_tokens: 5,
        cost_microunits: Some(7),
        accounting_status: ExecutionOutcomeUsageAccountingStatus::Known,
    });
    let message = ExecutionPortMessage::JobOutcomeMessage(outcome.clone());
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_settled_cleanup BEFORE UPDATE ON worker_session_slots WHEN NEW.state='completed' BEGIN SELECT RAISE(ABORT,'injected after original settlement'); END;").unwrap();
    assert!(fixture.accept(&message, at(20)).is_err());
    let stream = format!("execution-outcome:{}:1", job.job_id.0);
    assert_outcome_revision(&fixture, &stream, 1);
    let original_source = fixture
        .storage
        .execution_admission()
        .unwrap()
        .load_settlement_source(&job.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(original_source.fact.actual_tokens, 5);
    assert_eq!(original_source.fact.actual_cost_microunits, Some(7));
    let key = ActionEnforcementSigningKey::from_bytes([7; 32]).unwrap();
    let mut statement = settled_physical_statement(&outcome.lease, seed, complete);
    statement.sign(&key).unwrap();
    fixture
        .storage
        .reconcile_provider_attempt(&statement.verified(&key).unwrap(), &at(21))
        .unwrap();
    connection
        .execute_batch("DROP TRIGGER fail_settled_cleanup")
        .unwrap();
    drop(connection);
    fixture = fixture.restart();
    assert_eq!(
        outcome_status(&fixture.accept(&message, expired()).unwrap()),
        JobOutcomeAckMessageStatus::Duplicate
    );
    let ready = fixture.storage.load_state(&stream).unwrap().unwrap();
    assert_eq!(ready.revision, 2);
    let retained: Value = serde_json::from_slice(&ready.payload).unwrap();
    assert_eq!(retained["message"], serde_json::to_value(&outcome).unwrap());
    assert_eq!(
        fixture
            .storage
            .execution_admission()
            .unwrap()
            .load_settlement_source(&job.job_id)
            .unwrap()
            .unwrap(),
        original_source
    );
    let record = fixture.record();
    assert_eq!(
        record.turn_intents()[0]
            .terminal_outcome
            .as_ref()
            .unwrap()
            .usage,
        outcome.outcome.usage
    );
    let combined = fixture
        .storage
        .combine_execution_attempt_usage(&outcome.lease, outcome.outcome.usage.as_ref().unwrap())
        .unwrap();
    assert_eq!(combined.tokens, complete.then_some(16));
    assert_eq!(combined.cost_microunits, complete.then_some(9));
    assert_eq!(combined.known_tokens, if complete { 16 } else { 5 });
    assert_eq!(
        outcome_status(&fixture.accept(&message, expired()).unwrap()),
        JobOutcomeAckMessageStatus::Duplicate
    );
    assert_eq!(fixture.storage.load_state(&stream).unwrap().unwrap(), ready);
    let mut changed = outcome;
    changed.outcome.usage.as_mut().unwrap().tokens = Some(6);
    assert_outcome_conflict_without_write(
        &mut fixture,
        &changed,
        &ExecutionPortErrorCode::MessageConflict,
    );
    fixture.close();
}

fn original_cancel_ack(
    fixture: &mut Fixture,
    runtime: &RuntimeAuthority,
    message_seed: u64,
    status: JobCancelAckMessageStatus,
) -> (
    JobCancelAckMessage,
    winwincode_storage::RepositorySchedulerCancellationReceipt,
) {
    let queue = fixture
        .storage
        .load_execution_job_record(&runtime.lease.job_id)
        .unwrap()
        .unwrap();
    let request_id = queue.cancellation.as_ref().unwrap().request_id.clone();
    let scope = scheduler_scope(fixture);
    let source = fixture
        .storage
        .repository_scheduler()
        .unwrap()
        .load_cancellation_receipt(&scope, &request_id)
        .unwrap()
        .unwrap();
    let ExecutionPortMessage::JobCancelAckMessage(mut ack) = execution_message("job.cancel_ack")
    else {
        panic!("cancel acknowledgement fixture")
    };
    ack.error = None;
    ack.lease = runtime.lease.clone();
    ack.message_id = ExecutionMessageId(id("xmsg", message_seed));
    ack.request_id = source.request_id.clone();
    ack.sent_at = at(22);
    ack.session_identity = session_identity(fixture, runtime);
    ack.status = status;
    ack.worker_session_id = runtime.slot.worker_session_id.clone();
    (ack, source)
}

fn cancellation_ack_state(fixture: &Fixture) -> (Value, String) {
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    let (bytes, digest): (Vec<u8>, String) = connection.query_row(
        "SELECT s.payload, r.command_digest FROM product_state s JOIN command_receipts r ON r.stream_id=s.stream_id WHERE s.stream_id LIKE 'execution-cancellation-ack:%'",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    (serde_json::from_slice(&bytes).unwrap(), digest)
}

fn cancellation_ack_durable_counts(fixture: &Fixture) -> (i64, i64, i64) {
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    connection.query_row(
        "SELECT (SELECT COUNT(*) FROM command_receipts), (SELECT COUNT(*) FROM outbox), (SELECT COUNT(*) FROM execution_lease_terminals)",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap()
}

#[test]
#[allow(clippy::too_many_lines)]
fn cancel_ack_retains_original_control_without_terminalizing_unstarted_chat() {
    let seed = 96;
    let mut fixture = Fixture::open(seed);
    let job = create_chat_job(&mut fixture, seed);
    let scope = execution_scope(&fixture);
    register_worker(&mut fixture.storage, seed);
    reserve_execution(&mut fixture.storage, &scope, &job, seed);
    let dispatch = scheduler_claim(&mut fixture, seed, seed, 96_201, "boot-cancel-ack", 5);
    let session = WorkerSessionId(id("wsn", 96_202));
    accept_scheduler_dispatch(&mut fixture, &dispatch, &session, 96_203, 7);
    let runtime = open_scheduler_runtime(&mut fixture, &dispatch, 96_202, 96_204, 96_205, 8);
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(binding_message(
                &fixture, &runtime, 96_206,
            )),
            at(9),
        )
        .unwrap();
    assert!(
        fixture.record().bindings().is_empty(),
        "only the staged Worker binding exists"
    );
    cancel_bound_chat(&mut fixture);
    let (ack, source) = original_cancel_ack(
        &mut fixture,
        &runtime,
        96_207,
        JobCancelAckMessageStatus::Accepted,
    );
    let authority = product_authority_bytes(&mut fixture, &job, &[&session]);
    let resources = execution_resource_snapshot(&mut fixture, &job.job_id);
    let counts = cancellation_ack_durable_counts(&fixture);
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::JobCancelAckMessage(ack.clone()),
                at(22)
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        product_authority_bytes(&mut fixture, &job, &[&session]),
        authority
    );
    assert_eq!(
        execution_resource_snapshot(&mut fixture, &job.job_id),
        resources
    );
    assert_eq!(
        cancellation_ack_durable_counts(&fixture),
        (counts.0 + 1, counts.1 + 1, counts.2)
    );
    let (retained, digest) = cancellation_ack_state(&fixture);
    assert_eq!(retained["message"], serde_json::to_value(&ack).unwrap());
    assert_eq!(
        retained["originalCancelMessageId"],
        serde_json::to_value(&source.message_id).unwrap()
    );
    assert_eq!(
        retained["cancellation"],
        serde_json::to_value(&source.job.cancellation).unwrap()
    );
    assert_eq!(
        digest,
        format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&ack).unwrap())
        )
    );
    assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Cancelling);
    assert!(
        fixture.record().turn_intents()[0]
            .terminal_outcome
            .is_none()
    );
    let mut outcome = successful_outcome(&fixture, &runtime, 96_208);
    outcome.outcome.status = ExecutionOutcomeStatus::Cancelled;
    outcome.outcome.finished_at = at(23);
    outcome.sent_at = at(23);
    assert_eq!(
        outcome_status(
            &fixture
                .accept(&ExecutionPortMessage::JobOutcomeMessage(outcome), at(23))
                .unwrap()
        ),
        JobOutcomeAckMessageStatus::Accepted
    );
    assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Failed);
    assert_slot_state(&mut fixture, &runtime, WorkerSlotState::Cancelled);
    fixture.close();
}

#[test]
fn cancel_ack_negative_status_is_retained_without_cancellation_success() {
    for (index, status) in [
        JobCancelAckMessageStatus::RejectedExpiredLease,
        JobCancelAckMessageStatus::RejectedStaleFencingToken,
        JobCancelAckMessageStatus::RejectedWorkerInstance,
    ]
    .into_iter()
    .enumerate()
    {
        let seed = 97 + index as u64;
        let BoundSchedulerFixture {
            mut fixture,
            job,
            runtime,
            ..
        } = bound_scheduler_fixture(seed);
        cancel_bound_chat(&mut fixture);
        let (mut ack, _) = original_cancel_ack(&mut fixture, &runtime, seed * 1000, status);
        if index == 1 {
            ack.error = Some(winwincode_execution_port::generated::ExecutionPortError {
                code: ExecutionPortErrorCode::StaleFencingToken,
                message: "Worker observed a rejected cancellation control".into(),
                retryable: false,
            });
        }
        let authority =
            product_authority_bytes(&mut fixture, &job, &[&runtime.slot.worker_session_id]);
        let resources = execution_resource_snapshot(&mut fixture, &job.job_id);
        assert!(
            fixture
                .accept(
                    &ExecutionPortMessage::JobCancelAckMessage(ack.clone()),
                    at(22)
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            cancellation_ack_state(&fixture).0["message"],
            serde_json::to_value(&ack).unwrap()
        );
        assert_eq!(
            product_authority_bytes(&mut fixture, &job, &[&runtime.slot.worker_session_id]),
            authority
        );
        assert_eq!(
            execution_resource_snapshot(&mut fixture, &job.job_id),
            resources
        );
        assert!(
            fixture.record().turn_intents()[0]
                .terminal_outcome
                .is_none()
        );
        let scope = scheduler_scope(&fixture);
        let pending = RepositoryExecutionScheduler::new(&mut fixture.storage)
            .pending_cancellations(&scope)
            .unwrap();
        assert_eq!(
            pending.len(),
            1,
            "a negative ACK does not confirm consumption"
        );
        assert_eq!(pending[0].request_id, ack.request_id);
        register_worker_process(&mut fixture.storage, seed, seed + 10, seed * 1000 + 1, 24);
        let counts = cancellation_ack_durable_counts(&fixture);
        let message = ExecutionPortMessage::JobCancelAckMessage(ack.clone());
        assert!(fixture.accept(&message, expired()).unwrap().is_empty());
        assert_eq!(
            cancellation_ack_durable_counts(&fixture),
            counts,
            "accepted exact replay ignores replacement and expiry"
        );
        ack.message_id = ExecutionMessageId(id("xmsg", seed * 1000 + 2));
        assert!(
            fixture
                .accept(&ExecutionPortMessage::JobCancelAckMessage(ack), expired())
                .is_err(),
            "old owner cannot introduce a fresh ACK"
        );
        assert_eq!(cancellation_ack_durable_counts(&fixture), counts);
        fixture.close();
    }
}

#[test]
fn cancel_ack_stops_original_control_retransmission_across_restart() {
    for (index, status) in [
        JobCancelAckMessageStatus::Accepted,
        JobCancelAckMessageStatus::AlreadyCancelling,
        JobCancelAckMessageStatus::AlreadyTerminal,
    ]
    .into_iter()
    .enumerate()
    {
        let seed = 106 + index as u64;
        let BoundSchedulerFixture {
            mut fixture,
            job,
            runtime,
            ..
        } = bound_scheduler_fixture(seed);
        cancel_bound_chat(&mut fixture);
        let scope = scheduler_scope(&fixture);
        let original = RepositoryExecutionScheduler::new(&mut fixture.storage)
            .pending_cancellations(&scope)
            .unwrap();
        assert_eq!(original.len(), 1);
        let (ack, source) = original_cancel_ack(&mut fixture, &runtime, seed * 1000, status);
        assert_eq!(source.message_id.as_ref(), Some(&original[0].message_id));
        let authority =
            product_authority_bytes(&mut fixture, &job, &[&runtime.slot.worker_session_id]);
        let resources = execution_resource_snapshot(&mut fixture, &job.job_id);
        fixture
            .accept(
                &ExecutionPortMessage::JobCancelAckMessage(ack.clone()),
                at(22),
            )
            .unwrap();
        assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Cancelling);
        assert_eq!(
            product_authority_bytes(&mut fixture, &job, &[&runtime.slot.worker_session_id]),
            authority
        );
        assert_eq!(
            execution_resource_snapshot(&mut fixture, &job.job_id),
            resources
        );
        assert_eq!(cancellation_ack_durable_counts(&fixture).2, 0);
        assert!(
            RepositoryExecutionScheduler::new(&mut fixture.storage)
                .pending_cancellations(&scope)
                .unwrap()
                .is_empty()
        );
        let mut fixture = fixture.restart();
        let counts = cancellation_ack_durable_counts(&fixture);
        for _ in 0..3 {
            assert!(
                RepositoryExecutionScheduler::new(&mut fixture.storage)
                    .pending_cancellations(&scope)
                    .unwrap()
                    .is_empty()
            );
        }
        fixture
            .accept(&ExecutionPortMessage::JobCancelAckMessage(ack), at(22))
            .unwrap();
        assert_eq!(cancellation_ack_durable_counts(&fixture), counts);
        let mut outcome = successful_outcome(&fixture, &runtime, seed * 1000 + 1);
        outcome.outcome.status = ExecutionOutcomeStatus::Cancelled;
        outcome.outcome.finished_at = at(23);
        outcome.sent_at = at(23);
        assert_eq!(
            outcome_status(
                &fixture
                    .accept(&ExecutionPortMessage::JobOutcomeMessage(outcome), at(23))
                    .unwrap()
            ),
            JobOutcomeAckMessageStatus::Accepted
        );
        assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Failed);
        assert_slot_state(&mut fixture, &runtime, WorkerSlotState::Cancelled);
        assert_eq!(cancellation_ack_durable_counts(&fixture).2, 1);
        fixture.close();
    }
}

#[test]
fn cancel_ack_with_error_keeps_original_control_pending() {
    let seed = 110;
    let BoundSchedulerFixture {
        mut fixture,
        runtime,
        ..
    } = bound_scheduler_fixture(seed);
    cancel_bound_chat(&mut fixture);
    let scope = scheduler_scope(&fixture);
    let original = RepositoryExecutionScheduler::new(&mut fixture.storage)
        .pending_cancellations(&scope)
        .unwrap();
    let (mut ack, _) = original_cancel_ack(
        &mut fixture,
        &runtime,
        110_201,
        JobCancelAckMessageStatus::Accepted,
    );
    ack.error = Some(winwincode_execution_port::generated::ExecutionPortError {
        code: ExecutionPortErrorCode::MessageConflict,
        message: "Worker could not confirm cancellation cleanup".into(),
        retryable: false,
    });
    fixture
        .accept(&ExecutionPortMessage::JobCancelAckMessage(ack), at(22))
        .unwrap();
    assert_eq!(
        RepositoryExecutionScheduler::new(&mut fixture.storage)
            .pending_cancellations(&scope)
            .unwrap(),
        original
    );
    assert_eq!(cancellation_ack_durable_counts(&fixture).2, 0);
    fixture.close();
}

#[test]
fn cancel_control_consumption_and_ack_receipt_commit_atomically() {
    let seed = 109;
    let BoundSchedulerFixture {
        mut fixture,
        job,
        runtime,
        ..
    } = bound_scheduler_fixture(seed);
    cancel_bound_chat(&mut fixture);
    let scope = scheduler_scope(&fixture);
    let original = RepositoryExecutionScheduler::new(&mut fixture.storage)
        .pending_cancellations(&scope)
        .unwrap();
    let (ack, _) = original_cancel_ack(
        &mut fixture,
        &runtime,
        109_201,
        JobCancelAckMessageStatus::Accepted,
    );
    let counts = cancellation_ack_durable_counts(&fixture);
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_control_consumption BEFORE INSERT ON product_state WHEN NEW.stream_id LIKE 'execution-cancellation-consumed:%' BEGIN SELECT RAISE(ABORT,'injected control consumption failure'); END;").unwrap();
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::JobCancelAckMessage(ack.clone()),
                at(22)
            )
            .is_err()
    );
    assert_eq!(cancellation_ack_durable_counts(&fixture), counts);
    assert_eq!(
        RepositoryExecutionScheduler::new(&mut fixture.storage)
            .pending_cancellations(&scope)
            .unwrap(),
        original
    );
    connection
        .execute_batch("DROP TRIGGER fail_control_consumption")
        .unwrap();
    fixture
        .accept(
            &ExecutionPortMessage::JobCancelAckMessage(ack.clone()),
            at(22),
        )
        .unwrap();
    assert!(
        RepositoryExecutionScheduler::new(&mut fixture.storage)
            .pending_cancellations(&scope)
            .unwrap()
            .is_empty()
    );
    assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Cancelling);
    let retained: Vec<u8> = connection.query_row("SELECT payload FROM product_state WHERE stream_id LIKE 'execution-cancellation-consumed:%'", [], |row| row.get(0)).unwrap();
    let mut later = ack;
    later.message_id = ExecutionMessageId(id("xmsg", 109_202));
    later.status = JobCancelAckMessageStatus::AlreadyCancelling;
    fixture
        .accept(&ExecutionPortMessage::JobCancelAckMessage(later), at(23))
        .unwrap();
    let unchanged: Vec<u8> = connection.query_row("SELECT payload FROM product_state WHERE stream_id LIKE 'execution-cancellation-consumed:%'", [], |row| row.get(0)).unwrap();
    assert_eq!(
        unchanged, retained,
        "the first exact positive receipt remains authoritative"
    );
    connection.execute("UPDATE product_state SET payload=? WHERE stream_id LIKE 'execution-cancellation-consumed:%'", [b"{}".as_slice()]).unwrap();
    assert!(
        RepositoryExecutionScheduler::new(&mut fixture.storage)
            .pending_cancellations(&scope)
            .is_err(),
        "a corrupt reference cannot suppress or manufacture authority"
    );
    assert_eq!(cancellation_ack_durable_counts(&fixture).2, 0);
    drop(connection);
    fixture.close();
}

#[test]
fn cancel_ack_exact_replay_rejects_changed_raw_message_and_fresh_old_owner() {
    let seed = 100;
    let BoundSchedulerFixture {
        mut fixture,
        runtime,
        ..
    } = bound_scheduler_fixture(seed);
    cancel_bound_chat(&mut fixture);
    let (ack, _) = original_cancel_ack(
        &mut fixture,
        &runtime,
        100_201,
        JobCancelAckMessageStatus::Accepted,
    );
    let message = ExecutionPortMessage::JobCancelAckMessage(ack.clone());
    fixture.accept(&message, at(22)).unwrap();
    fixture = fixture.restart();
    register_worker_process(&mut fixture.storage, seed, seed + 1, 100_202, 24);
    let counts = cancellation_ack_durable_counts(&fixture);
    fixture.accept(&message, expired()).unwrap();
    for change in ["status", "sentAt", "request", "thread", "error"] {
        let mut changed = ack.clone();
        match change {
            "status" => changed.status = JobCancelAckMessageStatus::AlreadyCancelling,
            "sentAt" => changed.sent_at = at(23),
            "request" => changed.request_id = RequestId(id("req", 100_203)),
            "thread" => {
                changed.session_identity.codex_thread_id = CodexThreadId(id("cdx", 100_203));
            }
            _ => {
                changed.error = Some(winwincode_execution_port::generated::ExecutionPortError {
                    code: ExecutionPortErrorCode::MessageConflict,
                    message: "changed".into(),
                    retryable: false,
                });
            }
        }
        assert!(
            fixture
                .accept(
                    &ExecutionPortMessage::JobCancelAckMessage(changed),
                    expired()
                )
                .is_err(),
            "changed {change} cannot replay"
        );
    }
    let mut fresh = ack;
    fresh.message_id = ExecutionMessageId(id("xmsg", 100_204));
    assert!(
        fixture
            .accept(&ExecutionPortMessage::JobCancelAckMessage(fresh), expired())
            .is_err()
    );
    assert_eq!(cancellation_ack_durable_counts(&fixture), counts);
    fixture.close();
}

#[test]
fn already_terminal_cancel_ack_drains_after_original_job_outcome() {
    let seed = 101;
    let BoundSchedulerFixture {
        mut fixture,
        job,
        runtime,
        ..
    } = bound_scheduler_fixture(seed);
    cancel_bound_chat(&mut fixture);
    let (ack, _) = original_cancel_ack(
        &mut fixture,
        &runtime,
        101_201,
        JobCancelAckMessageStatus::AlreadyTerminal,
    );
    let mut outcome = successful_outcome(&fixture, &runtime, 101_202);
    outcome.outcome.status = ExecutionOutcomeStatus::Cancelled;
    outcome.outcome.finished_at = at(22);
    outcome.sent_at = at(22);
    fixture
        .accept(&ExecutionPortMessage::JobOutcomeMessage(outcome), at(22))
        .unwrap();
    let authority = product_authority_bytes(&mut fixture, &job, &[&runtime.slot.worker_session_id]);
    let resources = execution_resource_snapshot(&mut fixture, &job.job_id);
    let counts = cancellation_ack_durable_counts(&fixture);
    assert!(
        fixture
            .accept(&ExecutionPortMessage::JobCancelAckMessage(ack), at(23))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        product_authority_bytes(&mut fixture, &job, &[&runtime.slot.worker_session_id]),
        authority
    );
    assert_eq!(
        execution_resource_snapshot(&mut fixture, &job.job_id),
        resources
    );
    assert_eq!(
        cancellation_ack_durable_counts(&fixture),
        (counts.0 + 1, counts.1 + 1, counts.2)
    );
    fixture.close();
}

#[test]
fn cancel_ack_requires_original_control_and_canonical_error_shape() {
    let seed = 102;
    let BoundSchedulerFixture {
        mut fixture,
        runtime,
        ..
    } = bound_scheduler_fixture(seed);
    cancel_bound_chat(&mut fixture);
    let (ack, _) = original_cancel_ack(
        &mut fixture,
        &runtime,
        102_201,
        JobCancelAckMessageStatus::Accepted,
    );
    let counts = cancellation_ack_durable_counts(&fixture);
    for change in [
        "request",
        "thread",
        "scope",
        "period",
        "emptyError",
        "longError",
    ] {
        let mut changed = ack.clone();
        match change {
            "request" => changed.request_id = RequestId(id("req", 102_202)),
            "thread" => {
                changed.session_identity.codex_thread_id = CodexThreadId(id("cdx", 102_202));
            }
            "scope" => {
                changed.session_identity.product_session_id = ProductSessionId(id("psn", 102_202));
            }
            "period" => changed.lease.expires_at = at(29),
            field => {
                changed.error = Some(winwincode_execution_port::generated::ExecutionPortError {
                    code: ExecutionPortErrorCode::MessageConflict,
                    message: if field == "emptyError" {
                        String::new()
                    } else {
                        "x".repeat(501)
                    },
                    retryable: false,
                });
            }
        }
        assert!(
            fixture
                .accept(&ExecutionPortMessage::JobCancelAckMessage(changed), at(22))
                .is_err(),
            "invalid {change} cannot introduce a receipt"
        );
    }
    assert_eq!(cancellation_ack_durable_counts(&fixture), counts);
    fixture.close();
}

#[test]
fn cancel_ack_event_identity_separates_worker_local_message_counters() {
    let BoundSchedulerFixture {
        mut fixture,
        runtime: first_runtime,
        ..
    } = bound_scheduler_fixture(103);
    cancel_bound_chat(&mut fixture);
    let (first, _) = original_cancel_ack(
        &mut fixture,
        &first_runtime,
        1,
        JobCancelAckMessageStatus::Accepted,
    );
    fixture
        .accept(
            &ExecutionPortMessage::JobCancelAckMessage(first.clone()),
            at(22),
        )
        .unwrap();
    fixture.product_session_id = ProductSessionId(id("psn", 104));
    let second_job = create_chat_job(&mut fixture, 104);
    let scope = execution_scope(&fixture);
    register_worker(&mut fixture.storage, 104);
    reserve_execution(&mut fixture.storage, &scope, &second_job, 104);
    let BoundSchedulerFixture {
        mut fixture,
        runtime: second_runtime,
        ..
    } = finish_bound_scheduler_fixture(fixture, second_job, 104);
    let current = fixture.record();
    let mut context = command_context(
        &fixture.repository_scope,
        104_201,
        current.session().revision(),
    );
    context.occurred_at = at(21);
    ProductSessionService::new(&mut fixture.storage)
        .cancel_session(&CancelProductSessionCommand {
            context,
            product_session_id: fixture.product_session_id.clone(),
            actor: winwincode_session::AuthenticatedActor::User(UserId(id("usr", 1))),
            reason: "cancel the second real Worker job".into(),
        })
        .unwrap();
    let (second, _) = original_cancel_ack(
        &mut fixture,
        &second_runtime,
        1,
        JobCancelAckMessageStatus::Accepted,
    );
    assert_eq!(
        first.message_id, second.message_id,
        "message counters are local to each Worker"
    );
    assert_ne!(
        first.lease.worker_instance_id,
        second.lease.worker_instance_id
    );
    let counts = cancellation_ack_durable_counts(&fixture);
    fixture
        .accept(
            &ExecutionPortMessage::JobCancelAckMessage(second.clone()),
            at(22),
        )
        .unwrap();
    assert_eq!(
        cancellation_ack_durable_counts(&fixture),
        (counts.0 + 1, counts.1 + 1, counts.2)
    );
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    let (receipts, events): (i64, i64) = connection.query_row("SELECT (SELECT COUNT(*) FROM command_receipts WHERE stream_id LIKE 'execution-cancellation-ack:%'), (SELECT COUNT(DISTINCT event_id) FROM outbox WHERE topic='execution.job-cancellation-acknowledged.v1')", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!((receipts, events), (2, 2));
    drop(connection);
    let counts = cancellation_ack_durable_counts(&fixture);
    fixture
        .accept(&ExecutionPortMessage::JobCancelAckMessage(first), expired())
        .unwrap();
    fixture
        .accept(
            &ExecutionPortMessage::JobCancelAckMessage(second),
            expired(),
        )
        .unwrap();
    assert_eq!(cancellation_ack_durable_counts(&fixture), counts);
    fixture.close();
}

#[test]
#[allow(clippy::too_many_lines)]
fn unstarted_chat_terminal_projects_safe_assistant_once_after_durable_acceptance() {
    for status in [
        ExecutionOutcomeStatus::Cancelled,
        ExecutionOutcomeStatus::Failed,
        ExecutionOutcomeStatus::InfrastructureError,
    ] {
        let seed = 98;
        let mut fixture = Fixture::open(seed);
        let job = create_chat_job(&mut fixture, seed);
        let scope = execution_scope(&fixture);
        register_worker(&mut fixture.storage, seed);
        reserve_execution(&mut fixture.storage, &scope, &job, seed);
        let dispatch = scheduler_claim(
            &mut fixture,
            seed,
            seed,
            98_201,
            "boot-before-first-reply",
            5,
        );
        let worker_session_id = WorkerSessionId(id("wsn", 98_202));
        accept_scheduler_dispatch(&mut fixture, &dispatch, &worker_session_id, 98_203, 7);
        let runtime = open_scheduler_runtime(&mut fixture, &dispatch, 98_202, 98_204, 98_205, 8);
        fixture
            .accept(
                &ExecutionPortMessage::SessionBindingMessage(binding_message(
                    &fixture, &runtime, 98_206,
                )),
                at(9),
            )
            .unwrap();
        if status == ExecutionOutcomeStatus::Cancelled {
            cancel_bound_chat(&mut fixture);
        }
        let before = fixture.record();
        assert!(before.bindings().is_empty());
        assert!(before.turn_intents()[0].model_exchange_id.is_none());
        assert!(before.turn_intents()[0].terminal_outcome.is_none());
        assert_eq!(before.messages().len(), 1);

        let mut outcome = successful_outcome(&fixture, &runtime, 98_207);
        outcome.outcome.status = status.clone();
        outcome.outcome.usage =
            Some(winwincode_execution_port::generated::ExecutionOutcomeUsage::unknown(12_000, 0));
        outcome.outcome.error = Some(winwincode_execution_port::generated::ExecutionPortError {
            code: ExecutionPortErrorCode::MessageConflict,
            message: "private unstarted credential marker".into(),
            retryable: false,
        });
        let resources = execution_resource_snapshot(&mut fixture, &job.job_id);
        let authority =
            product_authority_bytes(&mut fixture, &job, &[&runtime.slot.worker_session_id]);
        let counts = cancellation_ack_durable_counts(&fixture);
        assert_eq!(outcome.lease.fencing_token.0, "1");
        let mut foreign_fence = outcome.clone();
        foreign_fence.lease.fencing_token = FencingToken("2".into());
        let rejected = fixture
            .accept(
                &ExecutionPortMessage::JobOutcomeMessage(foreign_fence.clone()),
                at(22),
            )
            .unwrap();
        let [ExecutionPortMessage::JobOutcomeAckMessage(ack)] = rejected.as_slice() else {
            panic!("one business rejection ACK");
        };
        assert_eq!(ack.status, JobOutcomeAckMessageStatus::RejectedConflict);
        assert_eq!(
            ack.error.as_ref().unwrap().code,
            ExecutionPortErrorCode::JobDispatchConflict
        );
        assert!(!ack.error.as_ref().unwrap().retryable);
        assert_eq!(ack.lease, foreign_fence.lease);
        assert_eq!(ack.worker_session_id, foreign_fence.worker_session_id);
        assert_eq!(ack.session_identity, foreign_fence.session_identity);
        assert_eq!(
            fixture.record(),
            before,
            "a rejected foreign fence has no public terminal"
        );
        assert_eq!(
            execution_resource_snapshot(&mut fixture, &job.job_id),
            resources
        );
        assert_eq!(
            product_authority_bytes(&mut fixture, &job, &[&runtime.slot.worker_session_id]),
            authority
        );
        assert_eq!(cancellation_ack_durable_counts(&fixture), counts);
        assert!(
            fixture
                .storage
                .load_state(&format!("execution-outcome:{}:1", job.job_id.0))
                .unwrap()
                .is_none()
        );
        let message = ExecutionPortMessage::JobOutcomeMessage(outcome.clone());
        assert_eq!(
            outcome_status(&fixture.accept(&message, at(22)).unwrap()),
            JobOutcomeAckMessageStatus::Accepted
        );
        let record = fixture.record();
        assert_eq!(record.messages().len(), 2);
        let assistant = record
            .messages()
            .iter()
            .find(|message| message.role == "assistant")
            .unwrap();
        assert_eq!(
            assistant.state,
            if status == ExecutionOutcomeStatus::Cancelled {
                "cancelled"
            } else {
                "failed"
            }
        );
        assert!(assistant.content.is_empty());
        assert!(assistant.attachments.is_none());
        assert!(assistant.artifact_refs.is_none());
        assert!(assistant.output_preparation.is_none());
        let mut identity = Sha256::new();
        identity.update(b"winwincode.chat-message.v1\0assistant\0");
        identity.update(job.job_id.0.as_bytes());
        let expected_id = format!("msg_{}", &format!("{:X}", identity.finalize())[..26]);
        assert_eq!(assistant.id.0, expected_id);
        assert!(record.bindings().is_empty());
        assert!(record.turn_intents()[0].model_exchange_id.is_none());
        assert_eq!(
            record.turn_intents()[0]
                .terminal_outcome
                .as_ref()
                .unwrap()
                .status,
            status
        );
        let public = serde_json::to_string(record.messages()).unwrap();
        assert!(!public.contains("private Worker summary marker"));
        assert!(!public.contains("private unstarted credential marker"));
        let stream = format!("execution-outcome:{}:1", job.job_id.0);
        let accepted = fixture.storage.load_state(&stream).unwrap().unwrap();
        let retained: Value = serde_json::from_slice(&accepted.payload).unwrap();
        assert_eq!(accepted.revision, 2);
        assert_eq!(retained["message"], serde_json::to_value(&outcome).unwrap());
        assert_eq!(retained["acknowledgementReady"], true);
        assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Failed);
        assert_slot_state(
            &mut fixture,
            &runtime,
            if status == ExecutionOutcomeStatus::Cancelled {
                WorkerSlotState::Cancelled
            } else {
                WorkerSlotState::Failed
            },
        );
        fixture = fixture.restart();
        assert_eq!(
            outcome_status(&fixture.accept(&message, expired()).unwrap()),
            JobOutcomeAckMessageStatus::Duplicate
        );
        assert_eq!(
            fixture.record(),
            record,
            "lost ACK replay creates no second assistant message"
        );
        assert_eq!(
            fixture.storage.load_state(&stream).unwrap().unwrap(),
            accepted
        );
        let resources = execution_resource_snapshot(&mut fixture, &job.job_id);
        let counts = cancellation_ack_durable_counts(&fixture);
        assert_eq!(
            outcome_status(
                &fixture
                    .accept(
                        &ExecutionPortMessage::JobOutcomeMessage(foreign_fence),
                        expired(),
                    )
                    .unwrap()
            ),
            JobOutcomeAckMessageStatus::RejectedConflict
        );
        assert_changed_terminal_frames_rejected(&mut fixture, &outcome);
        assert_eq!(fixture.record(), record);
        assert_eq!(
            execution_resource_snapshot(&mut fixture, &job.job_id),
            resources
        );
        assert_eq!(cancellation_ack_durable_counts(&fixture), counts);
        assert_eq!(
            fixture.storage.load_state(&stream).unwrap().unwrap(),
            accepted
        );
        let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM execution_lease_terminals WHERE job_id=?1",
                [&job.job_id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        drop(connection);
        fixture.close();
    }
}

// The retained historical message ledger is fixture setup. The current Job,
// authority, terminal, acceptance receipt and ACK are produced by the real ingress.
fn seed_legacy_chat_message_capacity(fixture: &Fixture, count: usize) {
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    let stream = format!(
        "product-sessions:{:x}",
        Sha256::digest(fixture.receipt_scope.as_bytes())
    );
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id=?1",
            [&stream],
            |row| row.get(0),
        )
        .unwrap();
    let mut catalog: Value = serde_json::from_slice(&bytes).unwrap();
    let messages = catalog["sessions"][&fixture.product_session_id.0]["messages"]
        .as_array_mut()
        .unwrap();
    assert_eq!(messages.len(), 1);
    let mut current = messages[0].clone();
    let mut history = Vec::with_capacity(count);
    for index in 0..count - 1 {
        let mut message = current.clone();
        message["projection"]["id"] = id("msg", 500_000 + u64::try_from(index).unwrap()).into();
        message["projection"]["sequence"] = (index + 1).into();
        message["projection"]["content"] = "retained historical user message".into();
        history.push(message);
    }
    current["projection"]["sequence"] = count.into();
    history.push(current);
    *messages = history;
    connection
        .execute(
            "UPDATE product_state SET payload=?1 WHERE stream_id=?2",
            (serde_json::to_vec(&catalog).unwrap(), &stream),
        )
        .unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn unstarted_terminal_preserves_full_legacy_ledger_and_reserves_new_projection_capacity() {
    for prior_count in [9_998, 10_000] {
        let seed = 99;
        let mut fixture = Fixture::open(seed);
        let job = create_chat_job(&mut fixture, seed);
        let scope = execution_scope(&fixture);
        register_worker(&mut fixture.storage, seed);
        reserve_execution(&mut fixture.storage, &scope, &job, seed);
        let dispatch = scheduler_claim(
            &mut fixture,
            seed,
            seed,
            99_201,
            "boot-legacy-message-capacity",
            5,
        );
        let worker_session = WorkerSessionId(id("wsn", 99_202));
        accept_scheduler_dispatch(&mut fixture, &dispatch, &worker_session, 99_203, 7);
        let runtime = open_scheduler_runtime(&mut fixture, &dispatch, 99_202, 99_204, 99_205, 8);
        fixture
            .accept(
                &ExecutionPortMessage::SessionBindingMessage(binding_message(
                    &fixture, &runtime, 99_206,
                )),
                at(9),
            )
            .unwrap();
        seed_legacy_chat_message_capacity(&fixture, prior_count);
        let history = fixture.record();
        assert_eq!(history.messages().len(), prior_count);
        let mut outcome = successful_outcome(&fixture, &runtime, 99_207);
        outcome.outcome.status = ExecutionOutcomeStatus::Failed;
        let message = ExecutionPortMessage::JobOutcomeMessage(outcome.clone());
        assert_eq!(
            outcome_status(&fixture.accept(&message, at(22)).unwrap()),
            JobOutcomeAckMessageStatus::Accepted
        );
        let settled = fixture.record();
        let expected_count = if prior_count == 10_000 {
            10_000
        } else {
            prior_count + 1
        };
        assert_eq!(settled.messages().len(), expected_count);
        assert_eq!(&settled.messages()[..prior_count], history.messages());
        assert!(settled.turn_intents()[0].model_exchange_id.is_none());
        assert_eq!(
            settled.turn_intents()[0]
                .terminal_outcome
                .as_ref()
                .unwrap()
                .status,
            ExecutionOutcomeStatus::Failed
        );
        assert_slot_state(&mut fixture, &runtime, WorkerSlotState::Failed);
        assert_queue_state(&fixture, &job.job_id, ExecutionJobState::Failed);
        let stream = format!("execution-outcome:{}:1", job.job_id.0);
        let accepted = fixture.storage.load_state(&stream).unwrap().unwrap();
        let retained: Value = serde_json::from_slice(&accepted.payload).unwrap();
        assert_eq!(accepted.revision, 2);
        assert_eq!(retained["acknowledgementReady"], true);
        fixture = fixture.restart();
        assert_eq!(
            outcome_status(&fixture.accept(&message, expired()).unwrap()),
            JobOutcomeAckMessageStatus::Duplicate
        );
        assert_eq!(fixture.record(), settled);
        assert_changed_terminal_frames_rejected(&mut fixture, &outcome);
        assert_eq!(fixture.record(), settled);
        let execution_config = ProductSessionExecutionConfig::try_new(
            fixture.repository_scope.clone(),
            "0123456789abcdef0123456789abcdef01234567",
            "codex-chat",
            Some(3_600),
            1_073_741_824,
        )
        .unwrap();
        let command = SubmitChatMessageCommand {
            context: command_context(
                &fixture.repository_scope,
                99_208,
                settled.session().revision(),
            ),
            product_session_id: fixture.product_session_id.clone(),
            attachments: Vec::new(),
            message: "another turn must reserve its terminal message".into(),
            execution_config,
        };
        assert_eq!(
            ProductSessionService::new(&mut fixture.storage)
                .submit_chat(&command)
                .unwrap_err()
                .code(),
            winwincode_control_plane::ProductSessionServiceErrorCode::MessageLimitExceeded
        );
        assert_eq!(
            fixture.record(),
            settled,
            "message capacity rejection cannot append the next user or Job"
        );
        assert_eq!(
            fixture.storage.load_state(&stream).unwrap().unwrap(),
            accepted
        );
        fixture.close();
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn cancellation_of_an_unbound_follow_up_uses_its_current_job_after_a_completed_binding() {
    let seed = 978;
    let AcceptedTerminalFixture {
        mut fixture,
        job: original_job,
        outcome,
        ..
    } = accepted_terminal_fixture(seed, false);
    let completed = fixture.record();
    let old_slot = fixture
        .storage
        .worker_session_slots()
        .unwrap()
        .load(
            completed.bindings()[0]
                .binding()
                .worker_session_id()
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(old_slot.state, WorkerSlotState::Completed);
    let execution_config = ProductSessionExecutionConfig::try_new(
        fixture.repository_scope.clone(),
        "0123456789abcdef0123456789abcdef01234567",
        "codex-chat",
        Some(3_600),
        1_073_741_824,
    )
    .unwrap();
    let follow_up = ProductSessionService::new(&mut fixture.storage)
        .submit_chat(&SubmitChatMessageCommand {
            context: command_context(
                &fixture.repository_scope,
                978_030,
                completed.session().revision(),
            ),
            product_session_id: fixture.product_session_id.clone(),
            message: "Hold the next turn before its first model reply".into(),
            attachments: Vec::new(),
            execution_config,
        })
        .unwrap();
    let scope = execution_scope(&fixture);
    let queued = fixture
        .storage
        .execution_queue()
        .unwrap()
        .load_job(&scope, &follow_up.turn_intent.execution_job_id)
        .unwrap()
        .unwrap();
    let next_job: ExecutionJob = serde_json::from_slice(&queued.dispatch_payload).unwrap();
    let next_seed = seed + 1;
    register_worker(&mut fixture.storage, next_seed);
    reserve_execution(&mut fixture.storage, &scope, &next_job, next_seed);
    transition_job(
        &mut fixture.storage,
        &scope,
        &next_job.job_id,
        978_031,
        1,
        ExecutionJobState::Queued,
        ExecutionJobState::Leased,
    );
    let runtime = claim_and_open_runtime(&mut fixture, &next_job, next_seed);
    accept_dispatch(&mut fixture, &next_job, &runtime, next_seed);
    let running = fixture.record();
    assert_eq!(running.bindings(), completed.bindings());
    assert_eq!(
        running.turn_intents()[1].state,
        winwincode_control_plane::ProductSessionTurnState::Pending
    );
    let mut cancel_context = command_context(
        &fixture.repository_scope,
        978_032,
        running.session().revision(),
    );
    cancel_context.occurred_at = at(18);
    let cancel = CancelProductSessionCommand {
        context: cancel_context,
        product_session_id: fixture.product_session_id.clone(),
        actor: winwincode_session::AuthenticatedActor::User(UserId(id("usr", 1))),
        reason: "cancel the current unbound follow-up".into(),
    };
    let mut stale = cancel.clone();
    stale.context.expected_revision = completed.session().revision();
    assert_eq!(
        ProductSessionService::new(&mut fixture.storage)
            .cancel_session(&stale)
            .unwrap_err()
            .code(),
        winwincode_control_plane::ProductSessionServiceErrorCode::RevisionConflict
    );
    let accepted = ProductSessionService::new(&mut fixture.storage)
        .cancel_session(&cancel)
        .expect("current accepted Job cancels despite the historical completed binding");
    assert_eq!(accepted.routing.routes.len(), 1);
    assert_eq!(
        accepted.routing.routes[0].job.execution_job_id,
        next_job.job_id
    );
    assert!(accepted.routing.routes[0].worker.is_none());
    let current = fixture
        .storage
        .execution_queue()
        .unwrap()
        .load_job(&scope, &next_job.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(current.state, ExecutionJobState::Cancelling);
    let original = fixture
        .storage
        .execution_queue()
        .unwrap()
        .load_job(&scope, &original_job.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(original.state, ExecutionJobState::Completed);
    assert!(original.cancellation.is_none());
    assert_eq!(
        fixture
            .storage
            .worker_session_slots()
            .unwrap()
            .load(&old_slot.authority.worker_session_id)
            .unwrap()
            .unwrap(),
        old_slot
    );
    let replay = ProductSessionService::new(&mut fixture.storage)
        .cancel_session(&cancel)
        .unwrap();
    assert_eq!(replay.routing.routes, accepted.routing.routes);
    let mut changed = cancel.clone();
    changed.reason = "changed cancel body".into();
    assert_eq!(
        ProductSessionService::new(&mut fixture.storage)
            .cancel_session(&changed)
            .unwrap_err()
            .code(),
        winwincode_control_plane::ProductSessionServiceErrorCode::RequestConflict
    );
    assert_eq!(
        outcome_status(
            &fixture
                .accept(&ExecutionPortMessage::JobOutcomeMessage(outcome), at(21))
                .unwrap()
        ),
        JobOutcomeAckMessageStatus::Duplicate
    );
    let after = fixture.record();
    assert_eq!(after.messages()[1], completed.messages()[1]);
    assert_eq!(after.turn_intents()[0], completed.turn_intents()[0]);
    fixture.close();
}
