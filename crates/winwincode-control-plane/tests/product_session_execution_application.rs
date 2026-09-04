// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, from_value};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, ModelRoute, OrganizationId, OrganizationScope,
    OrganizationScopeKind, ProjectId, ProviderAccountSource, RepositoryId, RepositoryScope,
    RepositoryScopeKind, Scope, SessionModelSelection, SystemDefaultProviderAccountSource,
    SystemDefaultProviderAccountSourceKind, UserActor, UserActorKind, WorkspaceId,
};
use winwincode_control_plane::{
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
    StandaloneModelExecutionError, StandaloneProviderConfig, SubmitChatMessageCommand,
    local_loopback_retry_policy,
};
use winwincode_domain::{
    CodexThreadId, ControlPlaneEventId, CredentialReferenceId, ExecutionEventId, ExecutionJobId,
    ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId, ProductSessionId,
    RequestId, Revision, SchemaVersion, SessionBindingSourceIdentity,
    SessionBindingSourceIdentityKind, SessionIdentity, Sha256Digest, UserId, WorkerId,
    WorkerInstanceId, WorkerSessionId,
};
use winwincode_execution_port::generated::{
    ExecutionEventCategory, ExecutionEventRecord, ExecutionJob, ExecutionLeaseStamp,
    ExecutionOutcomeStatus, ExecutionPortMessage, JobCancelMessage, JobCancelMessageKind,
    JobCancelMessageReason, JobDispatchResultMessage, JobDispatchResultMessageKind,
    JobDispatchResultMessageStatus, JobOutcomeAckMessageStatus, JobOutcomeMessage,
    LeaseWriteStatus, ModelOpenMessage, RuntimeEventMessage, RuntimeEventMessageKind,
    SessionBindingMessage,
};
use winwincode_execution_port::transport::{FrameDirection, TypedFrame};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionJobState, ExecutionJobTransitionRequest,
    ExecutionLeaseClaim, ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest,
    ExecutionQueueScope, ExecutionRepositoryAccess, ExecutionReservationRequest,
    ExecutionReservationStart, ExecutionReservationState, ProductStateStorage, PublicEventActor,
    PublicEventScope, ReceiptScopeKey, RepositorySchedulerClaimRequest,
    RepositorySchedulerRetryRequest, RepositorySchedulerScope, RepositorySchedulerTerminalRequest,
    SchedulerRetryPolicy, SqliteStorage, WorkerAuthenticationIdentity, WorkerHeartbeatRequest,
    WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest, WorkerSlotAuthority,
    WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources, WorkerSlotState,
    public_receipt_identity, receipt_scope_key,
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
            model_selection: SessionModelSelection {
                account_source: ProviderAccountSource::SystemDefaultProviderAccountSource(
                    SystemDefaultProviderAccountSource {
                        kind: SystemDefaultProviderAccountSourceKind::SystemDefault,
                    },
                ),
                model_id: "fixture-model".to_owned(),
                provider_id: "fixture-provider".to_owned(),
            },
        })
        .expect("create ProductSession");
    let execution_config = ProductSessionExecutionConfig::try_new(
        fixture.repository_scope.clone(),
        "0123456789abcdef0123456789abcdef01234567",
        "codex-chat",
        3_600,
        1_073_741_824,
    )
    .expect("execution config");
    let receipt = service
        .submit_chat(&SubmitChatMessageCommand {
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
            cost_budget_micros: 1_000_000,
        },
    )
    .expect("model admission policy");
    LocalModelPolicyAuthority::try_new(LocalModelPolicyAuthorityConfig {
        base,
        enterprise_ceilings: Vec::new(),
    })
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
        token_budget: 100_000,
        cost_budget_microunits: 1_000_000,
        max_runtime_millis: 60_000,
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
            reserved_tokens: 100,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 30_000,
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
        stage_run_id: None,
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
    message.stage_run_id = None;
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
    let result = fixture.accept(message, at(29));
    if let Ok(output) = result {
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
        .accept(&ExecutionPortMessage::ModelOpenMessage(open), expired())
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
            .accept(
                &ExecutionPortMessage::ModelOpenMessage(model_open.clone()),
                at(10),
            )
            .expect("attach model exchange")
            .is_empty()
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

fn accepted_terminal_fixture(seed: u64) -> AcceptedTerminalFixture {
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
            .accept(&ExecutionPortMessage::ModelOpenMessage(model_open), at(10),)
            .expect("attach model exchange")
            .is_empty()
    );
    let outcome = successful_outcome(&fixture, &runtime, seed);
    let output = fixture
        .accept(
            &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
            at(20),
        )
        .expect("first terminal outcome");
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
        .accept(&ExecutionPortMessage::ModelOpenMessage(open), at(10))
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
        .accept(&ExecutionPortMessage::ModelOpenMessage(open), at(17))
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
    let seed = 61;
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
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(original_binding.clone()),
            at(9),
        )
        .expect("original ProductSession binding");
    let original_open = model_open_message(&fixture, &original_runtime, 6_107);
    fixture
        .accept(
            &ExecutionPortMessage::ModelOpenMessage(original_open),
            at(10),
        )
        .expect("original ProductSession ModelOpen");

    register_worker_process(&mut fixture.storage, seed, seed + 1, 6_108, 11);
    let replacement_dispatch = scheduler_claim(&mut fixture, seed, seed + 1, 6_110, "boot-new", 25);
    assert_eq!(replacement_dispatch.job.job_id, job.job_id);
    assert_eq!(replacement_dispatch.job.attempt, 2);
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
    let mut replacement_open = model_open_message(&fixture, &replacement_runtime, 6_116);
    replacement_open.sent_at = at(28);
    fixture
        .accept(
            &ExecutionPortMessage::ModelOpenMessage(replacement_open.clone()),
            at(28),
        )
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

    assert_product_predecessor_is_fenced(
        &mut fixture,
        &job,
        &original_runtime,
        &replacement_runtime,
        6_117,
    );
    assert_product_replacement_replays_after_expiry(
        fixture,
        replacement_binding,
        replacement_open,
        &replacement_runtime,
    );
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
        .accept(&ExecutionPortMessage::ModelOpenMessage(open), at(28))
        .expect("fresh successor ModelOpen");

    let record = ProductSessionService::new(&mut fixture.storage)
        .get(&fixture.receipt_scope, &fixture.product_session_id)
        .expect("ProductSession read")
        .expect("ProductSession");
    assert_eq!(record.bindings().len(), 1);
    assert_eq!(record.bindings()[0].slot().authority, runtime.slot);
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
    } = accepted_terminal_fixture(seed);

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
