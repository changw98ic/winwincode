// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, ModelRoute, OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    CanonicalModelStreamFrame, ConfiguredModelRetryPlanAuthority, CredentialReferenceResolution,
    CredentialReferenceService, DurableEnterpriseQuotaAdmission, DurableModelExchangeAuthority,
    DurableModelRetryContextSource, DurableModelRetryPreOpenPlanner,
    DurableProviderGatewayAdmission, EnterpriseQuotaAdmission, EnterpriseQuotaAdmissionPort,
    FrozenModelRetryPlan, FrozenModelRouteAuthority, LocalModelPolicyAuthority,
    LocalModelPolicyAuthorityConfig, ModelAdmissionClock, ModelAdmissionClockError,
    ModelAdmissionLimits, ModelAdmissionPolicyLayer, ModelAdmissionService,
    ModelAttemptFailureFact, ModelAttemptFailureKind, ModelAttemptStartReceipt, ModelCapability,
    ModelExecutionCertainty, ModelExecutionOpenReceipt, ModelExecutionRuntime,
    ModelExecutionRuntimeErrorKind, ModelPolicyAuthorityError, ModelPolicyAuthorityPort,
    ModelPolicyAuthoritySnapshot, ModelPolicyRouteKey, ModelRequestPool, ModelRequestPoolConfig,
    ModelReservationReceipt, ModelReservationReleaseReason, ModelReservationTerminalOutcome,
    ModelReservationTerminalReceipt, ModelRetryPlannerError, ModelRetryPreOpenPlannerPort,
    ModelRetrySettlementContext, ModelRetrySettlementContextError, ModelRetrySettlementContextPort,
    ModelRetryStep, ModelRetryUsageRequest, ModelRoutePolicyDecision, ModelSettingsRequest,
    ModelSettingsService, ModelSettingsTarget, ModelSettingsValues, ModelToolSupport,
    ModelUsageAttribution, ProductStateStorage, ProviderAdapterError, ProviderAdapterInvocation,
    ProviderAdapterOpenReceipt, ProviderAdapterPort, ProviderAdmissionError,
    ProviderAdmissionOpenReceipt, ProviderAdmissionOpenRequest, ProviderAdmissionReservationConfig,
    ProviderCatalogRequest, ProviderCatalogService, ProviderDescriptor, ProviderFinishReason,
    ProviderGateway, ProviderGatewayAdmissionPort, ProviderGatewayErrorKind,
    ProviderGatewayIdentity, ProviderGatewayIdentityError, ProviderGatewayIdentityPort,
    ProviderGatewaySettlement, ProviderGatewaySettlementError, ProviderGatewaySettlementPort,
    ProviderGatewayTerminal, ProviderStreamControlAction, ProviderStreamConverter,
    ProviderStreamEvent, ProviderTokenUsage, ResolvedSecret, SecretStoreError, SecretStorePort,
    StructuredOutputSupport, command_receipt_identity,
};
use winwincode_domain::{
    CodexThreadId, CredentialReferenceId, DeliveryId, ExecutionAckSequence, ExecutionJobId,
    ExecutionMessageId, FencingToken, Instant, LeaseId, ModelExchangeId, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion, SessionIdentity,
    Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_execution_port::generated::{
    DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind, EncodedPayload, ExecutionJob,
    ExecutionLeaseStamp, ExecutionLimits, ExecutionPortError, ExecutionPortErrorCode,
    ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode, LeaseWriteStatus,
    ModelAckMessage, ModelAckMessageKind, ModelGatewayRoute, ModelOpenMessage,
    ModelOpenMessageKind,
};
use winwincode_storage::{
    EnterpriseQuotaReleaseReason, EnterpriseQuotaReservationState, EnterpriseQuotaTerminal,
    NewOutboxEvent, ProviderExchangeBegin, SqliteStorage, StateCommit,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-provider-gateway-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", 1)),
        kind: UserActorKind::User,
    })
}

fn organization_scope() -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", 1)),
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

fn model_capability(model_id: &str) -> ModelCapability {
    ModelCapability {
        model_id: model_id.to_owned(),
        display_name: format!("{model_id} display"),
        context_window_tokens: 128_000,
        max_output_tokens: 16_000,
        tool_support: ModelToolSupport::Parallel,
        structured_output_support: StructuredOutputSupport::Unsupported,
        reasoning_efforts: vec!["high".to_owned()],
    }
}

fn model_capability_with_structured_output(
    model_id: &str,
    structured_output_support: StructuredOutputSupport,
) -> ModelCapability {
    ModelCapability {
        structured_output_support,
        ..model_capability(model_id)
    }
}

fn register_provider(
    storage: &mut SqliteStorage,
    request_seed: u64,
    expected_catalog_version: u64,
    provider_id: &str,
    model_id: &str,
    credential_seed: u64,
) {
    register_provider_with_structured_output(
        storage,
        request_seed,
        expected_catalog_version,
        provider_id,
        model_id,
        credential_seed,
        StructuredOutputSupport::Unsupported,
    );
}

#[allow(clippy::too_many_arguments)]
fn register_provider_with_structured_output(
    storage: &mut SqliteStorage,
    request_seed: u64,
    expected_catalog_version: u64,
    provider_id: &str,
    model_id: &str,
    credential_seed: u64,
    structured_output_support: StructuredOutputSupport,
) {
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: Scope::OrganizationScope(organization_scope()),
                request_id: RequestId(id("req", request_seed)),
                expected_catalog_version,
            },
            &ProviderDescriptor {
                provider_id: provider_id.to_owned(),
                display_name: format!("{provider_id} display"),
                adapter_kind: "fixture-adapter".to_owned(),
                credential_reference_id: CredentialReferenceId(id("crd", credential_seed)),
                models: vec![model_capability_with_structured_output(
                    model_id,
                    structured_output_support,
                )],
            },
        )
        .expect("register Provider fixture");
}

fn create_credential(
    storage: &mut SqliteStorage,
    request_seed: u64,
    credential_seed: u64,
    provider_id: &str,
) {
    CredentialReferenceService::new(storage)
        .create(
            &CredentialReferenceCreateCommand {
                actor: actor(),
                command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
                expected_revision: Revision(0),
                payload: CredentialReferenceCreatePayload {
                    credential_reference_id: CredentialReferenceId(id("crd", credential_seed)),
                    display_name: format!("{provider_id} credential"),
                    provider_id: provider_id.to_owned(),
                    vault_locator: format!("local-fixture://{provider_id}"),
                },
                request_id: RequestId(id("req", request_seed)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::OrganizationScope(organization_scope()),
            },
            1_800_000_000_000 + request_seed,
        )
        .expect("create Credential reference fixture");
}

fn configure_session(
    storage: &mut SqliteStorage,
    request_seed: u64,
    product_session_seed: u64,
    provider_id: &str,
    model_id: &str,
    credential_seed: u64,
) {
    ModelSettingsService::new(storage)
        .update(
            &ModelSettingsRequest {
                actor: actor(),
                target: ModelSettingsTarget::ProductSession {
                    repository_scope: repository_scope(),
                    product_session_id: ProductSessionId(id("psn", product_session_seed)),
                },
                request_id: RequestId(id("req", request_seed)),
                expected_revision: 0,
            },
            ModelSettingsValues {
                default_model_route: Some(ModelRoute {
                    credential_reference_id: CredentialReferenceId(id("crd", credential_seed)),
                    provider_id: provider_id.to_owned(),
                    model_id: model_id.to_owned(),
                }),
                worker_concurrency_limit: 1,
            },
        )
        .expect("configure model session fixture");
}

fn encoded_payload(bytes: &[u8]) -> EncodedPayload {
    EncodedPayload {
        content_type: "application/json".to_owned(),
        data_base64: STANDARD.encode(bytes),
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
    }
}

fn open_message(
    message_seed: u64,
    exchange_seed: u64,
    request_seed: u64,
    product_session_seed: u64,
    payload: &[u8],
) -> ModelOpenMessage {
    let worker_session_id = WorkerSessionId(id("wsn", product_session_seed));
    ModelOpenMessage {
        kind: ModelOpenMessageKind::ModelOpen,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2030-01-01T00:05:00Z".to_owned()),
            fencing_token: FencingToken("1".to_owned()),
            issued_at: Instant("2030-01-01T00:00:00Z".to_owned()),
            job_id: ExecutionJobId(id("job", message_seed)),
            lease_id: LeaseId(id("lse", message_seed)),
            worker_id: WorkerId(id("wrk", message_seed)),
            worker_instance_id: WorkerInstanceId(id("wki", message_seed)),
        },
        message_id: ExecutionMessageId(id("xmsg", message_seed)),
        model_exchange_id: ModelExchangeId(id("mdl", exchange_seed)),
        request: encoded_payload(payload),
        request_id: RequestId(id("req", request_seed)),
        route: ModelGatewayRoute {
            capability: "reasoning".to_owned(),
            route: "configured-session-route".to_owned(),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2030-01-01T00:00:01Z".to_owned()),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId(id("cdx", product_session_seed)),
            product_session_id: ProductSessionId(id("psn", product_session_seed)),
            stage_run_id: None,
            worker_session_id: worker_session_id.clone(),
        },
        worker_session_id,
    }
}

fn commit_execution_job(storage: &mut SqliteStorage, message: &ModelOpenMessage) {
    let job = ExecutionJob {
        attempt: message.lease.attempt,
        execution_profile: "executor".to_owned(),
        goal: "execute the authenticated model request".to_owned(),
        job_id: message.lease.job_id.clone(),
        limits: ExecutionLimits {
            deadline_at: Instant("2030-01-01T00:05:00.000Z".to_owned()),
            max_artifact_bytes: 1_000_000,
            max_runtime_seconds: 300,
        },
        payload_digest: message.request.payload_digest.clone(),
        scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
            delivery_id: DeliveryId(id("dlv", 31)),
            delivery_task_id: None,
            kind: DeliveryStageExecutionScopeKind::DeliveryStage,
            product_session_id: message.session_identity.product_session_id.clone(),
            rework_authorization: None,
            stage_run_id: message
                .session_identity
                .stage_run_id
                .clone()
                .expect("stage run identity"),
        }),
        stage_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            repository_id: repository_scope().repository_id,
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    };
    let identity = command_receipt_identity(
        &actor(),
        &Scope::RepositoryScope(repository_scope()),
        RequestId(id("req", 880)),
    )
    .expect("ExecutionJob receipt identity");
    storage
        .commit(&StateCommit::new(
            identity,
            Sha256Digest(format!("sha256:{:064x}", 880)),
            "provider-planner-execution-job",
            0,
            br#"{"schema":"provider-planner-job.v1"}"#.to_vec(),
            vec![NewOutboxEvent::internal(
                format!("execution-job:{}", job.job_id.0),
                "execution.job.dispatch",
                serde_json::to_vec(&job).expect("ExecutionJob bytes"),
            )],
        ))
        .expect("commit authenticated ExecutionJob");
}

fn setup_planner_runtime(
    root: &Path,
    seed: u64,
    payload: &[u8],
) -> (SqliteStorage, ModelOpenMessage, FrozenModelRouteAuthority) {
    let mut storage = SqliteStorage::open(root).expect("open planner runtime storage");
    register_provider(&mut storage, seed, 0, "provider-a", "model-a", seed);
    create_credential(&mut storage, seed + 1, seed, "provider-a");
    configure_session(&mut storage, seed + 2, seed, "provider-a", "model-a", seed);
    let mut message = open_message(seed, seed, seed + 100, seed, payload);
    message.session_identity.stage_run_id = Some(StageRunId(id("run", seed)));
    commit_execution_job(&mut storage, &message);
    let route_authority = retry_context(&mut storage, &message).request().plan.steps()[0]
        .authority()
        .clone();
    (storage, message, route_authority)
}

fn cancellation_ack(message: &ModelOpenMessage, seed: u64) -> ModelAckMessage {
    ModelAckMessage {
        ack_sequence: ExecutionAckSequence(0),
        error: Some(ExecutionPortError {
            code: ExecutionPortErrorCode::Cancelled,
            message: "model exchange cancelled by Worker".to_owned(),
            retryable: false,
        }),
        kind: ModelAckMessageKind::ModelAck,
        lease: message.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", seed)),
        model_exchange_id: message.model_exchange_id.clone(),
        replay_from_sequence: None,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2030-01-01T00:00:02Z".to_owned()),
        session_identity: message.session_identity.clone(),
        status: LeaseWriteStatus::RejectedConflict,
        worker_session_id: message.worker_session_id.clone(),
    }
}

fn normal_ack(message: &ModelOpenMessage, seed: u64, sequence: i64) -> ModelAckMessage {
    let mut acknowledgement = cancellation_ack(message, seed);
    acknowledgement.ack_sequence = ExecutionAckSequence(sequence);
    acknowledgement.error = None;
    acknowledgement.status = LeaseWriteStatus::Accepted;
    acknowledgement
}

fn assert_current_and_expired_ack_authority(
    gateway: &ProviderGateway<'_>,
    message: &ModelOpenMessage,
) {
    gateway
        .validate_worker_acknowledgement(&normal_ack(message, 142, 1))
        .expect("current Worker ack authority");
    let mut expired = normal_ack(message, 143, 1);
    expired.sent_at = expired.lease.expires_at.clone();
    assert_eq!(
        gateway
            .validate_worker_acknowledgement(&expired)
            .expect_err("expired Worker ack is denied")
            .kind(),
        ProviderGatewayErrorKind::IdentityDenied
    );
}

fn route(provider_id: &str, model_id: &str, credential_seed: u64) -> ModelRoute {
    ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", credential_seed)),
        model_id: model_id.to_owned(),
        provider_id: provider_id.to_owned(),
    }
}

fn adapter_request_id(message: &ModelOpenMessage) -> String {
    format!("adapter_{}", message.message_id.0)
}

#[derive(Clone)]
struct FixedRetryContext(ModelRetrySettlementContext);

impl ModelRetrySettlementContextPort for FixedRetryContext {
    fn load_context(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ModelRetrySettlementContext>, ModelRetrySettlementContextError> {
        Ok(
            (self.0.start_receipt().model_exchange_id == *model_exchange_id)
                .then(|| self.0.clone()),
        )
    }
}

#[derive(Clone)]
struct FixedRetryPlanner(ModelRetrySettlementContext);

impl ModelRetryPreOpenPlannerPort for FixedRetryPlanner {
    fn prepare(
        &mut self,
        message: &ModelOpenMessage,
        admission: &ProviderAdmissionOpenReceipt,
    ) -> Result<ModelRetrySettlementContext, ModelRetryPlannerError> {
        assert_eq!(
            self.0.start_receipt().model_exchange_id,
            message.model_exchange_id
        );
        assert_eq!(
            self.0.start_receipt().reservation_request_id,
            message.request_id,
        );
        assert_eq!(
            self.0.start_receipt().route_fingerprint,
            admission.route_authority.fingerprint(),
        );
        Ok(self.0.clone())
    }
}

fn retry_context(
    storage: &mut SqliteStorage,
    message: &ModelOpenMessage,
) -> ModelRetrySettlementContext {
    let target = ModelSettingsTarget::ProductSession {
        repository_scope: repository_scope(),
        product_session_id: message.session_identity.product_session_id.clone(),
    };
    let projection = ModelSettingsService::new(storage)
        .project(&target)
        .expect("project model settings");
    let configured = projection
        .default_model_route
        .clone()
        .expect("configured model route");
    let scope = Scope::OrganizationScope(organization_scope());
    let capability = ProviderCatalogService::new(storage)
        .resolve_model(&scope, &configured.provider_id, &configured.model_id)
        .expect("resolve model capability");
    let credential = CredentialReferenceService::new(storage)
        .resolve(&scope, &configured.credential_reference_id)
        .expect("resolve Credential reference");
    let authority = FrozenModelRouteAuthority::from_resolved_authority(
        &ProviderGatewayIdentity::product_session(
            repository_scope(),
            message.session_identity.product_session_id.clone(),
        ),
        &projection,
        &capability,
        &credential,
    )
    .expect("freeze retry route authority");
    let plan = FrozenModelRetryPlan::freeze(
        "provider-runtime-retry".to_owned(),
        1,
        vec![ModelRetryStep::try_new(authority.clone(), 1).expect("retry step")],
    )
    .expect("freeze retry plan");
    let request = ModelRetryUsageRequest {
        request_id: RequestId(id("req", 990)),
        attribution: ModelUsageAttribution::from_request_authority(&authority, None, &actor())
            .expect("model Usage attribution"),
        plan,
        enterprise_quota_amounts: winwincode_storage::EnterpriseQuotaAmounts {
            tokens: 100,
            provider_cost_micros: 10,
            operations: 1,
            ..winwincode_storage::EnterpriseQuotaAmounts::default()
        },
        enterprise_quota_requested_at: Instant("2027-08-01T00:00:00.000Z".to_owned()),
    };
    let start = ModelAttemptStartReceipt {
        request_id: request.request_id.clone(),
        reservation_request_id: message.request_id.clone(),
        model_exchange_id: message.model_exchange_id.clone(),
        attempt: 1,
        provider_id: configured.provider_id,
        model_id: configured.model_id,
        route_fingerprint: authority.fingerprint().to_owned(),
        revision: 1,
        idempotent_replay: false,
    };
    ModelRetrySettlementContext::try_new(request, start).expect("retry settlement context")
}

fn runtime_open_identity(
    message: &ModelOpenMessage,
    context: &ModelRetrySettlementContext,
) -> (Sha256Digest, String) {
    let bytes = serde_json::to_vec(&(message, context.request_fingerprint()))
        .expect("runtime open identity JSON");
    let open_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)));
    let mut digest = Sha256::new();
    digest.update(b"winwincode.provider-adapter-request.v1\0");
    for value in [
        message.model_exchange_id.0.as_bytes(),
        message.request_id.0.as_bytes(),
        open_digest.0.as_bytes(),
    ] {
        digest.update(
            u64::try_from(value.len())
                .expect("fixture identity length fits u64")
                .to_be_bytes(),
        );
        digest.update(value);
    }
    (open_digest, format!("adapter_{:x}", digest.finalize()))
}

fn request_pool() -> ModelRequestPool {
    ModelRequestPool::new(ModelRequestPoolConfig {
        max_routes: 4,
        max_active_per_route: 1,
        max_waiting_per_route: 4,
        max_exchange_records_per_route: 8,
        max_buffered_frames_per_stream: 4,
        max_buffered_bytes_per_stream: 16 * 1024,
        resume_buffered_frames_per_stream: 1,
        resume_buffered_bytes_per_stream: 1024,
    })
    .expect("model request pool")
}

fn terminal_request_pool() -> ModelRequestPool {
    ModelRequestPool::new(ModelRequestPoolConfig {
        max_routes: 4,
        max_active_per_route: 1,
        max_waiting_per_route: 4,
        max_exchange_records_per_route: 8,
        max_buffered_frames_per_stream: 32,
        max_buffered_bytes_per_stream: 64 * 1024,
        resume_buffered_frames_per_stream: 8,
        resume_buffered_bytes_per_stream: 16 * 1024,
    })
    .expect("terminal model request pool")
}

fn pause_frames(
    receipt: &winwincode_control_plane::ProviderGatewayOpenReceipt,
) -> Vec<CanonicalModelStreamFrame> {
    let mut converter = ProviderStreamConverter::from_gateway_receipt(receipt);
    let mut events = vec![
        ProviderStreamEvent::ResponseStarted {
            provider_response_id: "response-runtime-pause".to_owned(),
        },
        ProviderStreamEvent::TextStarted { index: 0 },
    ];
    events.extend((0..8).map(|index| ProviderStreamEvent::TextDelta {
        index: 0,
        delta: format!("fragment-{index}"),
    }));
    events.push(ProviderStreamEvent::TextEnded { index: 0 });
    let frames = events
        .into_iter()
        .flat_map(|event| converter.ingest(event).expect("convert pause frame"))
        .take(4)
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 4);
    frames
}

fn terminal_frames(
    receipt: &winwincode_control_plane::ProviderGatewayOpenReceipt,
) -> Vec<CanonicalModelStreamFrame> {
    let mut converter = ProviderStreamConverter::from_gateway_receipt(receipt);
    [
        ProviderStreamEvent::ResponseStarted {
            provider_response_id: "response-runtime-terminal".to_owned(),
        },
        ProviderStreamEvent::TextStarted { index: 0 },
        ProviderStreamEvent::TextDelta {
            index: 0,
            delta: "complete".to_owned(),
        },
        ProviderStreamEvent::TextEnded { index: 0 },
        ProviderStreamEvent::Usage(usage()),
        ProviderStreamEvent::Finished(ProviderFinishReason::Stop),
    ]
    .into_iter()
    .flat_map(|event| converter.ingest(event).expect("convert terminal frame"))
    .collect()
}

struct RuntimeFixture {
    root: PathBuf,
    storage: SqliteStorage,
    message: ModelOpenMessage,
    planner: FixedRetryPlanner,
    context: FixedRetryContext,
    identity: FakeIdentity,
    secret_store: FakeSecretStore,
    settlement: SettlementProbe,
    admission: AdmissionProbe,
    probe: Arc<AdapterProbe>,
}

impl RuntimeFixture {
    fn new(name: &str) -> Self {
        let root = temporary_directory(name);
        let mut storage = SqliteStorage::open(&root).expect("open interrupted fixture storage");
        register_provider(&mut storage, 71, 0, "provider-a", "model-a", 1);
        create_credential(&mut storage, 72, 1, "provider-a");
        configure_session(&mut storage, 73, 1, "provider-a", "model-a", 1);
        let message = open_message(71, 71, 171, 1, br#"{"prompt":"interrupted"}"#);
        let context = FixedRetryContext(retry_context(&mut storage, &message));
        let planner = FixedRetryPlanner(context.0.clone());
        let identity = FakeIdentity {
            repository_scope: repository_scope(),
            deny: AtomicBool::new(false),
        };
        let secret = b"provider-a-secret-fixture".to_vec();
        let secret_store = FakeSecretStore {
            secrets: BTreeMap::from([("provider-a".to_owned(), secret.clone())]),
            resolutions: Mutex::new(Vec::new()),
        };
        let settlement = SettlementProbe::default();
        let admission = AdmissionProbe::default();
        let probe = Arc::new(AdapterProbe::default());
        Self {
            root,
            storage,
            message,
            planner,
            context,
            identity,
            secret_store,
            settlement,
            admission,
            probe,
        }
    }

    fn with_interrupted_open(mut self, adapter_opened: bool) -> Self {
        let (open_digest, adapter_request_id) =
            runtime_open_identity(&self.message, &self.context.0);
        self.storage
            .provider_exchange_store()
            .expect("provider exchange store")
            .begin_open(&ProviderExchangeBegin {
                model_exchange_id: self.message.model_exchange_id.clone(),
                request_id: self.message.request_id.clone(),
                message_id: self.message.message_id.clone(),
                open_digest,
                provider_id: "provider-a".to_owned(),
                adapter_request_id: adapter_request_id.clone(),
                started_at: self.message.sent_at.clone(),
            })
            .expect("precommit opening identity");
        if adapter_opened {
            let mut gateway = ProviderGateway::new(
                &mut self.storage,
                &self.secret_store,
                &self.identity,
                &self.settlement,
                &mut self.admission,
            );
            register_interrupted_adapter(
                &mut gateway,
                &self.probe,
                b"provider-a-secret-fixture".to_vec(),
            );
            gateway
                .open(
                    &self.message,
                    &route("provider-a", "model-a", 1),
                    &adapter_request_id,
                )
                .expect("Provider accepted before simulated crash");
        }
        self
    }

    fn recover(mut self, expected_provider_opens: u64) {
        let authority = DurableModelExchangeAuthority::open(&self.root)
            .expect("open durable exchange authority");
        {
            let mut gateway = ProviderGateway::new(
                &mut self.storage,
                &self.secret_store,
                &self.identity,
                &self.settlement,
                &mut self.admission,
            );
            register_interrupted_adapter(
                &mut gateway,
                &self.probe,
                b"provider-a-secret-fixture".to_vec(),
            );
            let mut pool = request_pool();
            let mut runtime = ModelExecutionRuntime::new(
                &authority,
                &mut self.planner,
                &self.context,
                &mut gateway,
                &mut pool,
            );
            assert_eq!(
                runtime
                    .open(&self.message)
                    .expect_err("Opening recovers to a stable failure")
                    .kind(),
                ModelExecutionRuntimeErrorKind::OpenInterrupted
            );
            assert_eq!(
                runtime
                    .open(&self.message)
                    .expect_err("exact failed replay is stable")
                    .kind(),
                ModelExecutionRuntimeErrorKind::OpenInterrupted
            );
        }
        authority.close().expect("close first authority");
        self.assert_restart_replay(ModelExecutionRuntimeErrorKind::OpenInterrupted);
        assert_eq!(
            self.probe.calls.load(Ordering::Relaxed),
            expected_provider_opens
        );
        assert_eq!(
            self.secret_store
                .resolutions
                .lock()
                .expect("lock SecretStore calls")
                .len(),
            usize::try_from(expected_provider_opens).expect("fixture count fits usize")
        );
        assert_eq!(
            self.probe
                .controls
                .lock()
                .expect("lock controls")
                .as_slice(),
            [
                ProviderStreamControlAction::Cancel,
                ProviderStreamControlAction::Release,
            ]
        );
        drop(self.storage);
        fs::remove_dir_all(self.root).expect("remove interrupted fixture");
    }

    fn assert_open_commit_failure(mut self) {
        let authority = DurableModelExchangeAuthority::open(&self.root)
            .expect("open durable exchange authority");
        Connection::open(authority.database_path())
            .expect("open failure injection connection")
            .execute_batch(
                "CREATE TRIGGER fail_provider_opened
                 BEFORE UPDATE OF state ON internal_provider_exchanges
                 WHEN NEW.state = 'opened'
                 BEGIN SELECT RAISE(FAIL, 'injected opened commit failure'); END;",
            )
            .expect("install opened commit failure");
        {
            let mut gateway = ProviderGateway::new(
                &mut self.storage,
                &self.secret_store,
                &self.identity,
                &self.settlement,
                &mut self.admission,
            );
            register_interrupted_adapter(
                &mut gateway,
                &self.probe,
                b"provider-a-secret-fixture".to_vec(),
            );
            let mut pool = request_pool();
            let mut runtime = ModelExecutionRuntime::new(
                &authority,
                &mut self.planner,
                &self.context,
                &mut gateway,
                &mut pool,
            );
            assert_eq!(
                runtime
                    .open(&self.message)
                    .expect_err("opened commit failure is surfaced")
                    .kind(),
                ModelExecutionRuntimeErrorKind::Storage
            );
            assert_eq!(
                runtime
                    .open(&self.message)
                    .expect_err("failed open replays without Provider")
                    .kind(),
                ModelExecutionRuntimeErrorKind::OpenFailed
            );
        }
        authority.close().expect("close first authority");
        self.assert_restart_replay(ModelExecutionRuntimeErrorKind::OpenFailed);
        assert_eq!(self.probe.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            self.secret_store
                .resolutions
                .lock()
                .expect("lock SecretStore calls")
                .len(),
            1
        );
        assert_eq!(
            self.probe
                .controls
                .lock()
                .expect("lock controls")
                .as_slice(),
            [
                ProviderStreamControlAction::Cancel,
                ProviderStreamControlAction::Release,
            ]
        );
        drop(self.storage);
        fs::remove_dir_all(self.root).expect("remove opened failure fixture");
    }

    fn assert_pause_authority_failure_recovers(mut self) {
        let authority = DurableModelExchangeAuthority::open(&self.root)
            .expect("open durable exchange authority");
        let frames = {
            let mut gateway = ProviderGateway::new(
                &mut self.storage,
                &self.secret_store,
                &self.identity,
                &self.settlement,
                &mut self.admission,
            );
            register_interrupted_adapter(
                &mut gateway,
                &self.probe,
                b"provider-a-secret-fixture".to_vec(),
            );
            let mut pool = request_pool();
            let mut runtime = ModelExecutionRuntime::new(
                &authority,
                &mut self.planner,
                &self.context,
                &mut gateway,
                &mut pool,
            );
            let ModelExecutionOpenReceipt::Opened { gateway, .. } = runtime
                .open(&self.message)
                .expect("open runtime Provider exchange")
            else {
                panic!("runtime request must start immediately");
            };
            let frames = pause_frames(&gateway);
            Connection::open(authority.database_path())
                .expect("open pool failure connection")
                .execute_batch(
                    "CREATE TRIGGER fail_pool_authority_update
                     BEFORE UPDATE ON internal_model_request_pool_authority
                     BEGIN SELECT RAISE(FAIL, 'injected pool authority failure'); END;",
                )
                .expect("install pool authority failure");
            assert_eq!(
                runtime
                    .offer_provider_batch(
                        &self.message.model_exchange_id,
                        &frames,
                        None,
                        &self.message.sent_at,
                    )
                    .expect_err("pause authority write fails after Provider Pause")
                    .kind(),
                ModelExecutionRuntimeErrorKind::Storage
            );
            frames
        };
        drop(authority);
        Connection::open(self.storage.database_path())
            .expect("open trigger cleanup connection")
            .execute_batch("DROP TRIGGER fail_pool_authority_update;")
            .expect("remove pool authority failure");
        self.assert_pause_restart(&frames);
        assert_eq!(self.probe.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            self.probe
                .controls
                .lock()
                .expect("lock controls")
                .as_slice(),
            [
                ProviderStreamControlAction::Pause,
                ProviderStreamControlAction::Resume,
                ProviderStreamControlAction::Pause,
                ProviderStreamControlAction::Resume,
            ]
        );
        drop(self.storage);
        fs::remove_dir_all(self.root).expect("remove pause failure fixture");
    }

    fn assert_pause_restart(&mut self, frames: &[CanonicalModelStreamFrame]) {
        let authority = DurableModelExchangeAuthority::open(&self.root)
            .expect("restart durable exchange authority");
        let mut gateway = ProviderGateway::new(
            &mut self.storage,
            &self.secret_store,
            &self.identity,
            &self.settlement,
            &mut self.admission,
        );
        register_interrupted_adapter(
            &mut gateway,
            &self.probe,
            b"provider-a-secret-fixture".to_vec(),
        );
        let mut pool = request_pool();
        let mut runtime = ModelExecutionRuntime::new(
            &authority,
            &mut self.planner,
            &self.context,
            &mut gateway,
            &mut pool,
        );
        assert!(matches!(
            runtime.open(&self.message).expect("restart exact open"),
            ModelExecutionOpenReceipt::Opened {
                idempotent_replay: true,
                ..
            }
        ));
        runtime
            .offer_provider_batch(
                &self.message.model_exchange_id,
                frames,
                None,
                &self.message.sent_at,
            )
            .expect("retry exact Provider batch");
        runtime
            .acknowledge(&normal_ack(&self.message, 172, 4))
            .expect("ack to low watermark resumes Provider");
        authority.close().expect("close pause authority");
    }

    fn assert_terminal_final_ack_recovery(mut self) {
        let authority = DurableModelExchangeAuthority::open(&self.root)
            .expect("open durable exchange authority");
        let final_sequence = {
            let mut gateway = ProviderGateway::new(
                &mut self.storage,
                &self.secret_store,
                &self.identity,
                &self.settlement,
                &mut self.admission,
            );
            register_interrupted_adapter(
                &mut gateway,
                &self.probe,
                b"provider-a-secret-fixture".to_vec(),
            );
            let mut pool = terminal_request_pool();
            let mut runtime = ModelExecutionRuntime::new(
                &authority,
                &mut self.planner,
                &self.context,
                &mut gateway,
                &mut pool,
            );
            let ModelExecutionOpenReceipt::Opened { gateway, .. } = runtime
                .open(&self.message)
                .expect("open terminal runtime exchange")
            else {
                panic!("terminal runtime request must start immediately");
            };
            let frames = terminal_frames(&gateway);
            let final_sequence = frames.last().expect("terminal frame").sequence();
            runtime
                .offer_provider_batch(
                    &self.message.model_exchange_id,
                    &frames,
                    Some(ProviderGatewayTerminal::Completed {
                        usage: usage(),
                        actual_cost_micros: 15,
                    }),
                    &self.message.sent_at,
                )
                .expect("persist terminal frames and receipt");
            final_sequence
        };
        drop(authority);
        let acknowledgement = normal_ack(
            &self.message,
            173,
            i64::try_from(final_sequence).expect("final sequence fits i64"),
        );
        let first = self.ack_after_final_ack_write_failure(&acknowledgement);
        let replay = self
            .restarted_ack(&acknowledgement)
            .expect("replay lost final-ack response");
        assert_terminal_ack_replay(first, replay);
        let mut changed = acknowledgement.clone();
        changed.ack_sequence.0 -= 1;
        assert_eq!(
            self.restarted_ack(&changed)
                .expect_err("changed final ack conflicts")
                .kind(),
            ModelExecutionRuntimeErrorKind::ExchangeConflict
        );
        let mut foreign = acknowledgement;
        foreign.lease.worker_id = WorkerId(id("wrk", 999));
        assert_eq!(
            self.restarted_ack(&foreign)
                .expect_err("foreign final ack is denied")
                .kind(),
            ModelExecutionRuntimeErrorKind::Gateway
        );
        assert_eq!(self.probe.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            self.probe
                .controls
                .lock()
                .expect("lock controls")
                .as_slice(),
            [ProviderStreamControlAction::Release]
        );
        drop(self.storage);
        fs::remove_dir_all(self.root).expect("remove terminal fixture");
    }

    fn ack_after_final_ack_write_failure(
        &mut self,
        acknowledgement: &ModelAckMessage,
    ) -> winwincode_control_plane::ModelExecutionAckReceipt {
        let authority = DurableModelExchangeAuthority::open(&self.root)
            .expect("restart durable exchange authority");
        let result = {
            let mut gateway = ProviderGateway::new(
                &mut self.storage,
                &self.secret_store,
                &self.identity,
                &self.settlement,
                &mut self.admission,
            );
            register_interrupted_adapter(
                &mut gateway,
                &self.probe,
                b"provider-a-secret-fixture".to_vec(),
            );
            let mut pool = terminal_request_pool();
            let mut runtime = ModelExecutionRuntime::new(
                &authority,
                &mut self.planner,
                &self.context,
                &mut gateway,
                &mut pool,
            );
            let connection = Connection::open(authority.database_path())
                .expect("open final-ack failure connection");
            connection
                .execute_batch(
                    "CREATE TRIGGER fail_runtime_final_ack BEFORE INSERT
                     ON internal_provider_exchange_final_acks
                     BEGIN SELECT RAISE(ABORT, 'injected final ack failure'); END;",
                )
                .expect("install final-ack failure");
            assert_eq!(
                runtime
                    .acknowledge(acknowledgement)
                    .expect_err("final-ack write failure is surfaced")
                    .kind(),
                ModelExecutionRuntimeErrorKind::Storage
            );
            connection
                .execute_batch("DROP TRIGGER fail_runtime_final_ack;")
                .expect("remove final-ack failure");
            runtime
                .acknowledge(acknowledgement)
                .expect("same-process exact final ack retry")
        };
        authority.close().expect("close ack authority");
        result
    }

    fn restarted_ack(
        &mut self,
        acknowledgement: &ModelAckMessage,
    ) -> Result<
        winwincode_control_plane::ModelExecutionAckReceipt,
        winwincode_control_plane::ModelExecutionRuntimeError,
    > {
        let authority = DurableModelExchangeAuthority::open(&self.root)
            .expect("restart durable exchange authority");
        let result = {
            let mut gateway = ProviderGateway::new(
                &mut self.storage,
                &self.secret_store,
                &self.identity,
                &self.settlement,
                &mut self.admission,
            );
            register_interrupted_adapter(
                &mut gateway,
                &self.probe,
                b"provider-a-secret-fixture".to_vec(),
            );
            let mut pool = terminal_request_pool();
            ModelExecutionRuntime::new(
                &authority,
                &mut self.planner,
                &self.context,
                &mut gateway,
                &mut pool,
            )
            .acknowledge(acknowledgement)
        };
        authority.close().expect("close ack authority");
        result
    }

    fn assert_restart_replay(&mut self, expected: ModelExecutionRuntimeErrorKind) {
        let authority = DurableModelExchangeAuthority::open(&self.root)
            .expect("restart durable exchange authority");
        let mut gateway = ProviderGateway::new(
            &mut self.storage,
            &self.secret_store,
            &self.identity,
            &self.settlement,
            &mut self.admission,
        );
        register_interrupted_adapter(
            &mut gateway,
            &self.probe,
            b"provider-a-secret-fixture".to_vec(),
        );
        let mut pool = request_pool();
        let mut runtime = ModelExecutionRuntime::new(
            &authority,
            &mut self.planner,
            &self.context,
            &mut gateway,
            &mut pool,
        );
        assert_eq!(
            runtime
                .open(&self.message)
                .expect_err("restart replays stable interrupted failure")
                .kind(),
            expected
        );
        authority.close().expect("close restarted authority");
    }
}

fn register_interrupted_adapter(
    gateway: &mut ProviderGateway<'_>,
    probe: &Arc<AdapterProbe>,
    secret: Vec<u8>,
) {
    gateway
        .register_adapter(Box::new(FakeAdapter {
            provider_id: "provider-a".to_owned(),
            expected_secret: secret,
            receipt_id: None,
            probe: Arc::clone(probe),
        }))
        .expect("register interrupted-open adapter");
}

#[test]
fn opening_restart_before_provider_call_fences_the_precommitted_identity() {
    RuntimeFixture::new("opening-before-provider")
        .with_interrupted_open(false)
        .recover(0);
}

#[test]
fn opening_restart_after_provider_acceptance_cancels_and_releases_once() {
    RuntimeFixture::new("opening-after-provider")
        .with_interrupted_open(true)
        .recover(1);
}

#[test]
fn accepted_open_commit_failure_cancels_releases_and_replays_stable_failure() {
    RuntimeFixture::new("opened-commit-failure").assert_open_commit_failure();
}

#[test]
fn pause_side_effect_then_authority_failure_forces_resume_after_restart() {
    RuntimeFixture::new("pause-authority-failure").assert_pause_authority_failure_recovers();
}

fn assert_terminal_ack_replay(
    first: winwincode_control_plane::ModelExecutionAckReceipt,
    replay: winwincode_control_plane::ModelExecutionAckReceipt,
) {
    let winwincode_control_plane::ModelExecutionAckReceipt::Acknowledged(first) = first else {
        panic!("terminal acknowledgement must not cancel");
    };
    let winwincode_control_plane::ModelExecutionAckReceipt::Acknowledged(replay) = replay else {
        panic!("terminal replay must not cancel");
    };
    assert!(!first.pool.replayed);
    assert!(replay.pool.replayed);
    assert_eq!(
        first.pool.acknowledged_sequence,
        replay.pool.acknowledged_sequence
    );
    assert_eq!(replay.pool.buffered_frames, 0);
}

#[test]
fn terminal_chunk_restart_final_ack_forgets_and_replays_lost_response() {
    RuntimeFixture::new("terminal-final-ack").assert_terminal_final_ack_recovery();
}

struct FakeIdentity {
    repository_scope: RepositoryScope,
    deny: AtomicBool,
}

impl ProviderGatewayIdentityPort for FakeIdentity {
    fn authorize(
        &self,
        message: &ModelOpenMessage,
    ) -> Result<ProviderGatewayIdentity, ProviderGatewayIdentityError> {
        if self.deny.load(Ordering::Relaxed) {
            return Err(ProviderGatewayIdentityError::denied());
        }
        Ok(ProviderGatewayIdentity::product_session(
            self.repository_scope.clone(),
            message.session_identity.product_session_id.clone(),
        ))
    }
}

#[derive(Default)]
struct FakeSecretStore {
    secrets: BTreeMap<String, Vec<u8>>,
    resolutions: Mutex<Vec<String>>,
}

impl SecretStorePort for FakeSecretStore {
    fn resolve(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        self.resolutions
            .lock()
            .expect("lock SecretStore calls")
            .push(reference.provider_id().to_owned());
        let bytes = self
            .secrets
            .get(reference.provider_id())
            .ok_or_else(SecretStoreError::missing)?
            .clone();
        ResolvedSecret::from_bytes(bytes)
    }
}

#[derive(Default)]
struct AdapterProbe {
    calls: AtomicU64,
    controls: Mutex<Vec<ProviderStreamControlAction>>,
    request_digests: Mutex<Vec<[u8; 32]>>,
    debug_output: Mutex<Vec<String>>,
}

struct FakeAdapter {
    provider_id: String,
    expected_secret: Vec<u8>,
    receipt_id: Option<String>,
    probe: Arc<AdapterProbe>,
}

impl ProviderAdapterPort for FakeAdapter {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn open(
        &self,
        invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        assert_eq!(credential.expose(), self.expected_secret);
        assert_eq!(invocation.content_type(), "application/json");
        assert!(invocation.model_exchange_id().0.starts_with("mdl_"));
        assert!(invocation.request_id().0.starts_with("req_"));
        assert!(!invocation.model_id().is_empty());
        self.probe.calls.fetch_add(1, Ordering::Relaxed);
        self.probe
            .request_digests
            .lock()
            .expect("lock adapter requests")
            .push(Sha256::digest(invocation.payload()).into());
        self.probe
            .debug_output
            .lock()
            .expect("lock adapter Debug output")
            .push(format!("{invocation:?}"));
        ProviderAdapterOpenReceipt::try_new(
            self.receipt_id
                .clone()
                .unwrap_or_else(|| invocation.adapter_request_id().to_owned()),
        )
    }

    fn control(
        &self,
        model_exchange_id: &ModelExchangeId,
        adapter_request_id: &str,
        action: ProviderStreamControlAction,
    ) -> Result<(), ProviderAdapterError> {
        assert!(model_exchange_id.0.starts_with("mdl_"));
        assert!(!adapter_request_id.is_empty());
        self.probe
            .controls
            .lock()
            .expect("lock adapter controls")
            .push(action);
        Ok(())
    }
}

#[derive(Default)]
struct SettlementProbe {
    attempts: AtomicU64,
    fail_next: AtomicBool,
    accepted: Mutex<Vec<ProviderGatewaySettlement>>,
}

impl ProviderGatewaySettlementPort for SettlementProbe {
    fn settle(
        &self,
        settlement: &ProviderGatewaySettlement,
    ) -> Result<(), ProviderGatewaySettlementError> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        if self.fail_next.swap(false, Ordering::Relaxed) {
            return Err(ProviderGatewaySettlementError);
        }
        self.accepted
            .lock()
            .expect("lock settlement calls")
            .push(settlement.clone());
        Ok(())
    }
}

#[derive(Default)]
struct AdmissionProbe {
    reserves: AtomicU64,
    terminals: AtomicU64,
    reserved: Mutex<BTreeMap<String, ProviderAdmissionOpenReceipt>>,
}

impl ProviderGatewayAdmissionPort for AdmissionProbe {
    fn reserve(
        &mut self,
        request: &ProviderAdmissionOpenRequest<'_>,
    ) -> Result<ProviderAdmissionOpenReceipt, ProviderAdmissionError> {
        let authority = FrozenModelRouteAuthority::from_resolved_authority(
            request.identity,
            request.settings,
            request.capability,
            request.credential,
        )
        .expect("Gateway admission authority");
        let mut reserved = self.reserved.lock().expect("lock reservations");
        if let Some(existing) = reserved.get(&request.message.model_exchange_id.0) {
            assert_eq!(existing.route_authority, authority);
            assert_eq!(existing.reservation.request_id, request.message.request_id);
            let mut replay = existing.clone();
            replay.reservation.idempotent_replay = true;
            return Ok(replay);
        }
        let revision = self.reserves.fetch_add(1, Ordering::Relaxed) + 1;
        let receipt = ProviderAdmissionOpenReceipt {
            reservation: ModelReservationReceipt {
                request_id: request.message.request_id.clone(),
                model_exchange_id: request.message.model_exchange_id.clone(),
                route_authority_fingerprint: authority.fingerprint().to_owned(),
                denial: None,
                unix_minute: 1,
                revision,
                idempotent_replay: false,
            },
            route_authority: authority,
            enterprise_quota_amounts: winwincode_storage::EnterpriseQuotaAmounts {
                tokens: 100,
                provider_cost_micros: 10,
                operations: 1,
                ..winwincode_storage::EnterpriseQuotaAmounts::default()
            },
        };
        reserved.insert(request.message.model_exchange_id.0.clone(), receipt.clone());
        Ok(receipt)
    }

    fn release(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        reason: ModelReservationReleaseReason,
    ) -> Result<ModelReservationTerminalReceipt, ProviderAdmissionError> {
        self.reserved
            .lock()
            .expect("lock reservations")
            .remove(&model_exchange_id.0);
        let revision = self.terminals.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(ModelReservationTerminalReceipt {
            request_id: original_request_id.clone(),
            model_exchange_id: model_exchange_id.clone(),
            route_authority_fingerprint: authority.fingerprint().to_owned(),
            outcome: match reason {
                ModelReservationReleaseReason::Cancelled => {
                    ModelReservationTerminalOutcome::Cancelled
                }
                ModelReservationReleaseReason::ProviderFailed => {
                    ModelReservationTerminalOutcome::ProviderFailed
                }
            },
            actual_tokens: 0,
            actual_cost_micros: 0,
            revision,
            idempotent_replay: revision > 1,
        })
    }

    fn release_if_reserved(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        reason: ModelReservationReleaseReason,
    ) -> Result<Option<ModelReservationTerminalReceipt>, ProviderAdmissionError> {
        if !self
            .reserved
            .lock()
            .expect("lock reservations")
            .contains_key(&model_exchange_id.0)
        {
            return Ok(None);
        }
        self.release(authority, original_request_id, model_exchange_id, reason)
            .map(Some)
    }

    fn complete(
        &mut self,
        authority: &FrozenModelRouteAuthority,
        original_request_id: &RequestId,
        model_exchange_id: &ModelExchangeId,
        usage: ProviderTokenUsage,
        actual_cost_micros: u64,
    ) -> Result<ModelReservationTerminalReceipt, ProviderAdmissionError> {
        let revision = self.terminals.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(ModelReservationTerminalReceipt {
            request_id: original_request_id.clone(),
            model_exchange_id: model_exchange_id.clone(),
            route_authority_fingerprint: authority.fingerprint().to_owned(),
            outcome: ModelReservationTerminalOutcome::Completed,
            actual_tokens: usage.input_tokens + usage.output_tokens,
            actual_cost_micros,
            revision,
            idempotent_replay: revision > 1,
        })
    }
}

const fn usage() -> ProviderTokenUsage {
    ProviderTokenUsage {
        input_tokens: 10,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 5,
        reasoning_output_tokens: 0,
    }
}

fn expected_provider_usage_id(
    model_exchange_id: &ModelExchangeId,
    provider_id: &str,
    model_id: &str,
    adapter_request_id: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.provider-usage.v1\0");
    for value in [
        model_exchange_id.0.as_str(),
        provider_id,
        model_id,
        adapter_request_id,
    ] {
        digest.update(
            u64::try_from(value.len())
                .expect("fixture identity length fits u64")
                .to_be_bytes(),
        );
        digest.update(value.as_bytes());
    }
    format!("provider-usage:sha256:{:x}", digest.finalize())
}

struct GatewayClock;

impl ModelAdmissionClock for GatewayClock {
    fn unix_minute(&self) -> Result<u64, ModelAdmissionClockError> {
        Ok(31_556_300)
    }
}

struct UnavailablePolicyAuthority;

impl ModelPolicyAuthorityPort for UnavailablePolicyAuthority {
    fn snapshot(
        &self,
        _key: &ModelPolicyRouteKey,
    ) -> Result<ModelPolicyAuthoritySnapshot, ModelPolicyAuthorityError> {
        Err(ModelPolicyAuthorityError::unavailable())
    }
}

fn admission_policy(decision: ModelRoutePolicyDecision) -> ModelAdmissionPolicyLayer {
    ModelAdmissionPolicyLayer::try_new(
        "gateway-base-policy".to_owned(),
        1,
        "budget-2030-01".to_owned(),
        decision,
        ModelAdmissionLimits {
            requests_per_minute: 100,
            tokens_per_minute: 100_000,
            concurrent_requests: 100,
            token_budget: 1_000_000,
            cost_budget_micros: 1_000_000,
        },
    )
    .expect("Gateway policy fixture")
}

struct AdmissionOrderFixture<'fixture> {
    root: &'fixture Path,
    secret_store: &'fixture FakeSecretStore,
    identity: &'fixture FakeIdentity,
    settlement: &'fixture SettlementProbe,
    adapter_probe: &'fixture Arc<AdapterProbe>,
    reservation: ProviderAdmissionReservationConfig,
}

impl AdmissionOrderFixture<'_> {
    fn assert_failure(
        &self,
        storage: &mut SqliteStorage,
        authority: &dyn ModelPolicyAuthorityPort,
        message: &ModelOpenMessage,
        expected: ProviderGatewayErrorKind,
    ) {
        let admission_storage =
            SqliteStorage::open(self.root).expect("open policy admission storage");
        let mut admission = DurableProviderGatewayAdmission::new(
            admission_storage,
            &GatewayClock,
            authority,
            self.reservation,
        );
        {
            let mut gateway = ProviderGateway::new(
                storage,
                self.secret_store,
                self.identity,
                self.settlement,
                &mut admission,
            );
            gateway
                .register_adapter(Box::new(FakeAdapter {
                    provider_id: "provider-a".to_owned(),
                    expected_secret: b"provider-a-secret-fixture".to_vec(),
                    receipt_id: None,
                    probe: Arc::clone(self.adapter_probe),
                }))
                .expect("register Provider adapter");
            assert_eq!(
                gateway
                    .open(
                        message,
                        &route("provider-a", "model-a", 21),
                        &adapter_request_id(message),
                    )
                    .expect_err("production admission failure")
                    .kind(),
                expected
            );
        }
        admission.close().expect("close policy admission");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn route_secret_adapter_replay_and_settlement_boundaries_are_deterministic() {
    let root = temporary_directory("routing");
    let mut storage = SqliteStorage::open(&root).expect("open Gateway storage");
    register_provider(&mut storage, 1, 0, "provider-a", "model-a", 1);
    register_provider(&mut storage, 2, 1, "provider-b", "model-b", 2);
    create_credential(&mut storage, 3, 1, "provider-a");
    create_credential(&mut storage, 4, 2, "provider-b");
    configure_session(&mut storage, 5, 1, "provider-a", "model-a", 1);
    configure_session(&mut storage, 6, 2, "provider-b", "model-b", 2);

    let pending_before = storage
        .pending_events()
        .expect("read setup outbox before Gateway");
    let identity = FakeIdentity {
        repository_scope: repository_scope(),
        deny: AtomicBool::new(false),
    };
    let secret_a = b"provider-a-secret-fixture".to_vec();
    let secret_b = b"provider-b-secret-fixture".to_vec();
    let secret_store = FakeSecretStore {
        secrets: BTreeMap::from([
            ("provider-a".to_owned(), secret_a.clone()),
            ("provider-b".to_owned(), secret_b.clone()),
        ]),
        resolutions: Mutex::new(Vec::new()),
    };
    let settlement = SettlementProbe::default();
    let mut admission = AdmissionProbe::default();
    let probe_a = Arc::new(AdapterProbe::default());
    let probe_b = Arc::new(AdapterProbe::default());
    {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secret_store,
            &identity,
            &settlement,
            &mut admission,
        );
        gateway
            .register_adapter(Box::new(FakeAdapter {
                provider_id: "provider-a".to_owned(),
                expected_secret: secret_a.clone(),
                receipt_id: None,
                probe: Arc::clone(&probe_a),
            }))
            .expect("register Provider A adapter");
        gateway
            .register_adapter(Box::new(FakeAdapter {
                provider_id: "provider-b".to_owned(),
                expected_secret: secret_b,
                receipt_id: None,
                probe: Arc::clone(&probe_b),
            }))
            .expect("register Provider B adapter");

        let message_a = open_message(1, 1, 100, 1, br#"{"prompt":"alpha"}"#);
        let route_a = route("provider-a", "model-a", 1);
        let opened_a = gateway
            .open(&message_a, &route_a, &adapter_request_id(&message_a))
            .expect("open Provider A exchange");
        assert_eq!(opened_a.route, route_a);
        assert!(!opened_a.idempotent_replay);
        assert_eq!(probe_a.calls.load(Ordering::Relaxed), 1);
        assert_eq!(probe_b.calls.load(Ordering::Relaxed), 0);

        let paused = gateway
            .set_provider_read_paused(&message_a.model_exchange_id, true)
            .expect("pause Provider read");
        assert!(!paused.replayed);
        assert!(
            gateway
                .set_provider_read_paused(&message_a.model_exchange_id, true)
                .expect("replay pause")
                .replayed
        );
        let resumed = gateway
            .set_provider_read_paused(&message_a.model_exchange_id, false)
            .expect("resume Provider read");
        assert!(!resumed.replayed);
        assert!(
            gateway
                .set_provider_read_paused(&message_a.model_exchange_id, false)
                .expect("replay resume")
                .replayed
        );

        let replay = gateway
            .open(&message_a, &route_a, &adapter_request_id(&message_a))
            .expect("replay exact Provider A open");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.adapter_request_id, opened_a.adapter_request_id);
        assert_eq!(probe_a.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            secret_store.resolutions.lock().expect("lock calls").len(),
            1
        );

        let changed_replay = open_message(1, 1, 100, 1, br#"{"prompt":"changed"}"#);
        let conflict = gateway
            .open(
                &changed_replay,
                &route_a,
                &adapter_request_id(&changed_replay),
            )
            .expect_err("changed model exchange replay conflicts");
        assert_eq!(conflict.kind(), ProviderGatewayErrorKind::ExchangeConflict);
        assert_eq!(probe_a.calls.load(Ordering::Relaxed), 1);

        let mismatch_message = open_message(2, 2, 101, 1, br#"{"prompt":"route"}"#);
        let mismatched = gateway
            .open(
                &mismatch_message,
                &route("provider-b", "model-b", 2),
                &adapter_request_id(&mismatch_message),
            )
            .expect_err("Worker route cannot override configured session route");
        assert_eq!(mismatched.kind(), ProviderGatewayErrorKind::RouteMismatch);
        assert_eq!(
            secret_store.resolutions.lock().expect("lock calls").len(),
            1
        );

        let leak_payload = format!(
            "{{\"prompt\":\"{}\"}}",
            String::from_utf8(secret_a.clone()).expect("fixture secret is UTF-8")
        );
        let leak_message = open_message(3, 3, 102, 1, leak_payload.as_bytes());
        let leaked = gateway
            .open(&leak_message, &route_a, &adapter_request_id(&leak_message))
            .expect_err("request cannot contain resolved Credential bytes");
        assert_eq!(leaked.kind(), ProviderGatewayErrorKind::CredentialLeak);
        assert_eq!(probe_a.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            secret_store.resolutions.lock().expect("lock calls").len(),
            2
        );

        let message_b = open_message(4, 4, 103, 2, br#"{"prompt":"beta"}"#);
        let route_b = route("provider-b", "model-b", 2);
        gateway
            .open(&message_b, &route_b, &adapter_request_id(&message_b))
            .expect("open isolated Provider B exchange");
        assert_eq!(probe_a.calls.load(Ordering::Relaxed), 1);
        assert_eq!(probe_b.calls.load(Ordering::Relaxed), 1);

        identity.deny.store(true, Ordering::Relaxed);
        let denied_message = open_message(5, 5, 104, 1, br#"{"prompt":"denied"}"#);
        let denied = gateway
            .open(
                &denied_message,
                &route_a,
                &adapter_request_id(&denied_message),
            )
            .expect_err("identity denial precedes routing");
        assert_eq!(denied.kind(), ProviderGatewayErrorKind::IdentityDenied);
        identity.deny.store(false, Ordering::Relaxed);

        let first_terminal = gateway
            .apply_terminal(
                &message_a.model_exchange_id,
                ProviderGatewayTerminal::Completed {
                    usage: usage(),
                    actual_cost_micros: 15,
                },
                &message_a.sent_at,
            )
            .expect("settle Provider A exchange");
        assert!(!first_terminal.idempotent_replay);
        let replay_terminal = gateway
            .apply_terminal(
                &message_a.model_exchange_id,
                ProviderGatewayTerminal::Completed {
                    usage: usage(),
                    actual_cost_micros: 15,
                },
                &message_a.sent_at,
            )
            .expect("replay Provider A terminal");
        assert!(replay_terminal.idempotent_replay);
        assert_eq!(settlement.attempts.load(Ordering::Relaxed), 1);
        let terminal_conflict = gateway
            .apply_terminal(
                &message_a.model_exchange_id,
                ProviderGatewayTerminal::Failed {
                    failure: ModelAttemptFailureFact {
                        kind: ModelAttemptFailureKind::Transport,
                        certainty: ModelExecutionCertainty::NotSent,
                    },
                    charge: None,
                },
                &message_a.sent_at,
            )
            .expect_err("changed terminal conflicts");
        assert_eq!(
            terminal_conflict.kind(),
            ProviderGatewayErrorKind::TerminalConflict
        );
        let message_a2 = open_message(6, 6, 106, 1, br#"{"prompt":"alpha-two"}"#);
        let opened_a2 = gateway
            .open(&message_a2, &route_a, &adapter_request_id(&message_a2))
            .expect("open second Provider A exchange");
        assert_ne!(opened_a2.adapter_request_id, opened_a.adapter_request_id);
        gateway
            .apply_terminal(
                &message_a2.model_exchange_id,
                ProviderGatewayTerminal::Completed {
                    usage: usage(),
                    actual_cost_micros: 15,
                },
                &message_a2.sent_at,
            )
            .expect("settle second Provider A exchange");

        let cancellation = cancellation_ack(&message_b, 901);
        let mut stale_cancellation = cancellation.clone();
        stale_cancellation.worker_session_id = WorkerSessionId(id("wsn", 999));
        assert_eq!(
            gateway
                .cancel_from_worker(&stale_cancellation)
                .expect_err("stale Worker cancellation is denied")
                .kind(),
            ProviderGatewayErrorKind::IdentityDenied
        );
        settlement.fail_next.store(true, Ordering::Relaxed);
        let settlement_failure = gateway
            .cancel_from_worker(&cancellation)
            .expect_err("settlement failure remains retryable");
        assert_eq!(
            settlement_failure.kind(),
            ProviderGatewayErrorKind::SettlementUnavailable
        );
        gateway
            .cancel_from_worker(&cancellation)
            .expect("retry failed settlement");
        gateway
            .cancel_from_worker(&cancellation)
            .expect("replay settled Provider B terminal");
        assert_eq!(settlement.attempts.load(Ordering::Relaxed), 4);
    }

    let pending_after = storage.pending_events().expect("read outbox after Gateway");
    assert_eq!(pending_after, pending_before);
    assert_eq!(
        secret_store
            .resolutions
            .lock()
            .expect("lock SecretStore calls")
            .as_slice(),
        ["provider-a", "provider-a", "provider-b", "provider-a"]
    );
    assert!(
        probe_a
            .debug_output
            .lock()
            .expect("lock adapter Debug output")[0]
            .contains("[REDACTED]")
    );
    assert!(
        !probe_a
            .debug_output
            .lock()
            .expect("lock adapter Debug output")[0]
            .contains("alpha")
    );
    let accepted = settlement
        .accepted
        .lock()
        .expect("lock accepted settlements");
    assert_eq!(accepted.len(), 3);
    assert_eq!(accepted[0].provider_id, "provider-a");
    assert_eq!(
        accepted[0].settled_at,
        Instant("2030-01-01T00:00:01Z".to_owned())
    );
    assert_eq!(accepted[1].provider_id, "provider-a");
    assert_eq!(accepted[2].provider_id, "provider-b");
    assert_eq!(
        accepted[0]
            .charge
            .as_ref()
            .expect("completed settlement charge")
            .provider_usage_id,
        expected_provider_usage_id(
            &accepted[0].model_exchange_id,
            &accepted[0].provider_id,
            &accepted[0].model_id,
            &accepted[0].adapter_request_id,
        )
    );
    assert_ne!(
        accepted[0]
            .charge
            .as_ref()
            .expect("first exchange charge")
            .provider_usage_id,
        accepted[1]
            .charge
            .as_ref()
            .expect("second exchange charge")
            .provider_usage_id
    );
    assert_eq!(
        accepted[0]
            .charge
            .as_ref()
            .expect("completed settlement charge")
            .cost_micros,
        15
    );
    assert_eq!(
        accepted[2]
            .failure
            .expect("cancelled settlement failure")
            .kind,
        ModelAttemptFailureKind::Cancelled
    );
    assert_eq!(
        probe_a
            .controls
            .lock()
            .expect("lock Provider A controls")
            .as_slice(),
        [
            ProviderStreamControlAction::Pause,
            ProviderStreamControlAction::Resume,
            ProviderStreamControlAction::Release,
            ProviderStreamControlAction::Release,
        ]
    );
    assert_eq!(
        probe_b
            .controls
            .lock()
            .expect("lock Provider B controls")
            .as_slice(),
        [
            ProviderStreamControlAction::Cancel,
            ProviderStreamControlAction::Release,
        ]
    );

    fs::remove_dir_all(root).expect("remove Gateway fixture directory");
}

#[test]
fn restored_gateway_forces_the_durable_pool_read_decision_to_the_adapter() {
    let root = temporary_directory("restored-read-control");
    let mut storage = SqliteStorage::open(&root).expect("open Gateway storage");
    register_provider(&mut storage, 41, 0, "provider-a", "model-a", 1);
    create_credential(&mut storage, 42, 1, "provider-a");
    configure_session(&mut storage, 43, 1, "provider-a", "model-a", 1);
    let identity = FakeIdentity {
        repository_scope: repository_scope(),
        deny: AtomicBool::new(false),
    };
    let secret = b"provider-a-secret-fixture".to_vec();
    let secret_store = FakeSecretStore {
        secrets: BTreeMap::from([("provider-a".to_owned(), secret.clone())]),
        resolutions: Mutex::new(Vec::new()),
    };
    let settlement = SettlementProbe::default();
    let mut admission = AdmissionProbe::default();
    let probe = Arc::new(AdapterProbe::default());
    let message = open_message(41, 41, 141, 1, br#"{"prompt":"pause"}"#);
    let durable = {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secret_store,
            &identity,
            &settlement,
            &mut admission,
        );
        gateway
            .register_adapter(Box::new(FakeAdapter {
                provider_id: "provider-a".to_owned(),
                expected_secret: secret.clone(),
                receipt_id: None,
                probe: Arc::clone(&probe),
            }))
            .expect("register Provider adapter");
        gateway
            .open(
                &message,
                &route("provider-a", "model-a", 1),
                &adapter_request_id(&message),
            )
            .expect("open Provider exchange");
        gateway
            .set_provider_read_paused(&message.model_exchange_id, true)
            .expect("pause before crash");
        gateway
            .durable_exchange(&message.model_exchange_id)
            .expect("snapshot exchange")
    };
    let mut restarted = ProviderGateway::new(
        &mut storage,
        &secret_store,
        &identity,
        &settlement,
        &mut admission,
    );
    restarted
        .register_adapter(Box::new(FakeAdapter {
            provider_id: "provider-a".to_owned(),
            expected_secret: secret,
            receipt_id: None,
            probe: Arc::clone(&probe),
        }))
        .expect("register restarted adapter");
    restarted
        .restore_durable_exchange(&durable)
        .expect("restore durable exchange");
    assert_current_and_expired_ack_authority(&restarted, &message);
    assert!(
        !restarted
            .set_provider_read_paused(&message.model_exchange_id, false)
            .expect("force durable read decision after restart")
            .replayed
    );
    restarted
        .apply_terminal(
            &message.model_exchange_id,
            ProviderGatewayTerminal::Failed {
                failure: ModelAttemptFailureFact {
                    kind: ModelAttemptFailureKind::Transport,
                    certainty: ModelExecutionCertainty::AcceptanceUnknown,
                },
                charge: None,
            },
            &message.sent_at,
        )
        .expect("failed accepted open is cancelled before release");
    assert_eq!(
        probe.controls.lock().expect("lock controls").as_slice(),
        [
            ProviderStreamControlAction::Pause,
            ProviderStreamControlAction::Resume,
            ProviderStreamControlAction::Cancel,
            ProviderStreamControlAction::Release,
        ]
    );
    fs::remove_dir_all(root).expect("remove Gateway fixture directory");
}

#[test]
fn production_policy_deny_and_unavailability_precede_secret_and_provider_calls() {
    let root = temporary_directory("production-admission-order");
    let mut storage = SqliteStorage::open(&root).expect("open Gateway storage");
    register_provider(&mut storage, 21, 0, "provider-a", "model-a", 21);
    create_credential(&mut storage, 22, 21, "provider-a");
    configure_session(&mut storage, 23, 21, "provider-a", "model-a", 21);
    let identity = FakeIdentity {
        repository_scope: repository_scope(),
        deny: AtomicBool::new(false),
    };
    let secret_store = FakeSecretStore {
        secrets: BTreeMap::from([(
            "provider-a".to_owned(),
            b"provider-a-secret-fixture".to_vec(),
        )]),
        resolutions: Mutex::new(Vec::new()),
    };
    let settlement = SettlementProbe::default();
    let adapter_probe = Arc::new(AdapterProbe::default());
    let reservation =
        ProviderAdmissionReservationConfig::try_new(100, 10).expect("Provider reservation config");
    let denied_authority = LocalModelPolicyAuthority::try_new(LocalModelPolicyAuthorityConfig {
        base: admission_policy(ModelRoutePolicyDecision::Deny),
        enterprise_ceilings: Vec::new(),
    })
    .expect("denied production policy");
    let fixture = AdmissionOrderFixture {
        root: &root,
        secret_store: &secret_store,
        identity: &identity,
        settlement: &settlement,
        adapter_probe: &adapter_probe,
        reservation,
    };
    fixture.assert_failure(
        &mut storage,
        &denied_authority,
        &open_message(21, 21, 121, 21, br#"{"prompt":"denied"}"#),
        ProviderGatewayErrorKind::AdmissionDenied,
    );
    assert_eq!(adapter_probe.calls.load(Ordering::Relaxed), 0);
    assert!(
        secret_store
            .resolutions
            .lock()
            .expect("lock secret calls")
            .is_empty()
    );

    fixture.assert_failure(
        &mut storage,
        &UnavailablePolicyAuthority,
        &open_message(22, 22, 122, 21, br#"{"prompt":"unavailable"}"#),
        ProviderGatewayErrorKind::AdmissionUnavailable,
    );
    assert_eq!(adapter_probe.calls.load(Ordering::Relaxed), 0);
    assert!(
        secret_store
            .resolutions
            .lock()
            .expect("lock secret calls")
            .is_empty()
    );

    drop(storage);
    fs::remove_dir_all(root).expect("remove Gateway fixture directory");
}

#[test]
fn planner_context_failure_exact_releases_admission_without_secret_or_provider() {
    let root = temporary_directory("planner-release");
    let (mut storage, message, route_authority) =
        setup_planner_runtime(&root, 31, br#"{"prompt":"planner-fault"}"#);
    Connection::open(storage.database_path())
        .expect("open planner failure connection")
        .execute_batch(
            "CREATE TRIGGER fail_runtime_retry_context
             BEFORE INSERT ON product_state
             WHEN NEW.stream_id LIKE 'model-retry-context:%'
             BEGIN SELECT RAISE(ABORT, 'injected retry context failure'); END;",
        )
        .expect("install retry context failure");

    let model_policy = LocalModelPolicyAuthority::try_new(LocalModelPolicyAuthorityConfig {
        base: admission_policy(ModelRoutePolicyDecision::Allow),
        enterprise_ceilings: Vec::new(),
    })
    .expect("allow production policy");
    let admission_storage = SqliteStorage::open(&root).expect("open admission storage");
    let mut admission = DurableProviderGatewayAdmission::new(
        admission_storage,
        &GatewayClock,
        &model_policy,
        ProviderAdmissionReservationConfig::try_new(100, 10).expect("reservation estimate"),
    );
    let retry_policy = ConfiguredModelRetryPlanAuthority::try_new("runtime-retry".to_owned(), 1, 1)
        .expect("retry policy");
    let mut planner =
        DurableModelRetryPreOpenPlanner::open(&root, &retry_policy).expect("open planner");
    let contexts = DurableModelRetryContextSource::open(&root).expect("open context source");
    let exchanges = DurableModelExchangeAuthority::open(&root).expect("open exchange authority");
    let identity = FakeIdentity {
        repository_scope: repository_scope(),
        deny: AtomicBool::new(false),
    };
    let secrets = FakeSecretStore {
        secrets: BTreeMap::from([(
            "provider-a".to_owned(),
            b"provider-a-secret-fixture".to_vec(),
        )]),
        resolutions: Mutex::new(Vec::new()),
    };
    let settlement = SettlementProbe::default();
    let provider = Arc::new(AdapterProbe::default());
    {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secrets,
            &identity,
            &settlement,
            &mut admission,
        );
        register_interrupted_adapter(
            &mut gateway,
            &provider,
            b"provider-a-secret-fixture".to_vec(),
        );
        let mut pool = request_pool();
        let mut runtime = ModelExecutionRuntime::new(
            &exchanges,
            &mut planner,
            &contexts,
            &mut gateway,
            &mut pool,
        );
        for _attempt in 0..2 {
            assert_eq!(
                runtime
                    .open(&message)
                    .expect_err("planner failure remains terminal before Provider")
                    .kind(),
                ModelExecutionRuntimeErrorKind::Planning,
            );
        }
    }
    assert_provider_side_effects(&secrets, &provider, 0);
    assert_released_admission_budget(&root, &route_authority);

    exchanges.close().expect("close exchange authority");
    contexts.close().expect("close context source");
    planner.close().expect("close planner");
    admission.close().expect("close admission");
    drop(storage);
    fs::remove_dir_all(root).expect("remove planner release fixture");
}

#[test]
fn restart_after_reservation_before_prepare_replays_once_then_opens_provider() {
    let root = temporary_directory("planner-reserve-restart");
    let (mut storage, message, route_authority) =
        setup_planner_runtime(&root, 41, br#"{"prompt":"reserve-restart"}"#);
    let identity = FakeIdentity {
        repository_scope: repository_scope(),
        deny: AtomicBool::new(false),
    };
    let secrets = FakeSecretStore {
        secrets: BTreeMap::from([(
            "provider-a".to_owned(),
            b"provider-a-secret-fixture".to_vec(),
        )]),
        resolutions: Mutex::new(Vec::new()),
    };
    let settlement = SettlementProbe::default();
    let provider = Arc::new(AdapterProbe::default());
    let model_policy = LocalModelPolicyAuthority::try_new(LocalModelPolicyAuthorityConfig {
        base: admission_policy(ModelRoutePolicyDecision::Allow),
        enterprise_ceilings: Vec::new(),
    })
    .expect("allow production policy");
    let mut admission = DurableProviderGatewayAdmission::new(
        SqliteStorage::open(&root).expect("open first admission storage"),
        &GatewayClock,
        &model_policy,
        ProviderAdmissionReservationConfig::try_new(100, 10).expect("reservation estimate"),
    );
    {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secrets,
            &identity,
            &settlement,
            &mut admission,
        );
        register_interrupted_adapter(
            &mut gateway,
            &provider,
            b"provider-a-secret-fixture".to_vec(),
        );
        let reserved = gateway
            .reserve_before_open(&message)
            .expect("reserve before simulated crash");
        assert!(!reserved.reservation.idempotent_replay);
    }
    admission.close().expect("close first admission");
    drop(storage);

    let mut storage = SqliteStorage::open(&root).expect("restart planner storage");
    let mut admission = DurableProviderGatewayAdmission::new(
        SqliteStorage::open(&root).expect("restart admission storage"),
        &GatewayClock,
        &model_policy,
        ProviderAdmissionReservationConfig::try_new(100, 10).expect("reservation estimate"),
    );
    let retry_policy = ConfiguredModelRetryPlanAuthority::try_new("runtime-retry".to_owned(), 1, 1)
        .expect("retry policy");
    let mut planner =
        DurableModelRetryPreOpenPlanner::open(&root, &retry_policy).expect("restart planner");
    let contexts = DurableModelRetryContextSource::open(&root).expect("open context source");
    let exchanges = DurableModelExchangeAuthority::open(&root).expect("open exchange authority");
    {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secrets,
            &identity,
            &settlement,
            &mut admission,
        );
        register_interrupted_adapter(
            &mut gateway,
            &provider,
            b"provider-a-secret-fixture".to_vec(),
        );
        let mut pool = request_pool();
        let mut runtime = ModelExecutionRuntime::new(
            &exchanges,
            &mut planner,
            &contexts,
            &mut gateway,
            &mut pool,
        );
        assert!(matches!(
            runtime.open(&message).expect("resume pre-open reservation"),
            ModelExecutionOpenReceipt::Opened { .. }
        ));
    }
    assert_provider_side_effects(&secrets, &provider, 1);
    assert_active_admission_budget(&root, &route_authority);

    exchanges.close().expect("close exchange authority");
    contexts.close().expect("close context source");
    planner.close().expect("close planner");
    admission.close().expect("close admission");
    drop(storage);
    fs::remove_dir_all(root).expect("remove planner restart fixture");
}

fn assert_released_admission_budget(root: &Path, authority: &FrozenModelRouteAuthority) {
    let mut storage = SqliteStorage::open(root).expect("open admission inspection storage");
    let snapshot = ModelAdmissionService::new(&mut storage, &GatewayClock)
        .snapshot(authority, "budget-2030-01")
        .expect("load admission snapshot");
    assert_eq!(snapshot.active_reservations, 0);
    assert_eq!(snapshot.minute_requests, 1);
    assert_eq!(snapshot.minute_tokens, 0);
    assert_eq!(snapshot.budget_reserved_tokens, 0);
    assert_eq!(snapshot.budget_reserved_cost_micros, 0);
}

fn assert_provider_side_effects(
    secrets: &FakeSecretStore,
    provider: &AdapterProbe,
    expected: usize,
) {
    assert_eq!(
        secrets
            .resolutions
            .lock()
            .expect("lock secret resolutions")
            .len(),
        expected,
    );
    assert_eq!(
        provider.calls.load(Ordering::Relaxed),
        u64::try_from(expected).expect("side-effect count fits u64"),
    );
}

fn assert_active_admission_budget(root: &Path, authority: &FrozenModelRouteAuthority) {
    let mut storage = SqliteStorage::open(root).expect("open admission inspection storage");
    let snapshot = ModelAdmissionService::new(&mut storage, &GatewayClock)
        .snapshot(authority, "budget-2030-01")
        .expect("load admission snapshot");
    assert_eq!(snapshot.active_reservations, 1);
    assert_eq!(snapshot.minute_requests, 1);
    assert_eq!(snapshot.minute_tokens, 100);
    assert_eq!(snapshot.budget_reserved_tokens, 100);
    assert_eq!(snapshot.budget_reserved_cost_micros, 10);
}

#[test]
fn strict_json_schema_is_rejected_before_anthropic_credentials_admission_or_provider_io() {
    let root = temporary_directory("structured-output-preflight");
    let mut storage = SqliteStorage::open(&root).expect("open Gateway storage");
    register_provider_with_structured_output(
        &mut storage,
        81,
        0,
        "anthropic",
        "claude-fixture",
        81,
        StructuredOutputSupport::Unsupported,
    );
    // Deliberately omit CredentialReference state: capability routing must reject first.
    configure_session(&mut storage, 83, 81, "anthropic", "claude-fixture", 81);

    let identity = FakeIdentity {
        repository_scope: repository_scope(),
        deny: AtomicBool::new(false),
    };
    let secret_store = FakeSecretStore {
        secrets: BTreeMap::from([(
            "anthropic".to_owned(),
            b"anthropic-secret-must-not-be-read".to_vec(),
        )]),
        resolutions: Mutex::new(Vec::new()),
    };
    let settlement = SettlementProbe::default();
    let mut admission = AdmissionProbe::default();
    let probe = Arc::new(AdapterProbe::default());
    let message = open_message(
        81,
        81,
        181,
        81,
        br#"{"request":{"text":{"format":{"type":"json_schema","strict":true,"name":"change_batch","schema":{"type":"object"}}}}}"#,
    );
    {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secret_store,
            &identity,
            &settlement,
            &mut admission,
        );
        gateway
            .register_adapter(Box::new(FakeAdapter {
                provider_id: "anthropic".to_owned(),
                expected_secret: b"anthropic-secret-must-not-be-read".to_vec(),
                receipt_id: None,
                probe: Arc::clone(&probe),
            }))
            .expect("register Anthropic fixture adapter");

        let reserve_error = gateway
            .reserve_before_open(&message)
            .expect_err("unsupported capability must fail before reservation");
        assert_eq!(
            reserve_error.kind(),
            ProviderGatewayErrorKind::StructuredOutputUnsupported
        );
        let open_error = gateway
            .open(
                &message,
                &route("anthropic", "claude-fixture", 81),
                &adapter_request_id(&message),
            )
            .expect_err("unsupported capability must fail before Provider open");
        assert_eq!(
            open_error.kind(),
            ProviderGatewayErrorKind::StructuredOutputUnsupported
        );
        assert_eq!(
            ModelAttemptFailureFact::from_gateway(
                open_error.kind(),
                ModelExecutionCertainty::NotSent,
            )
            .kind,
            ModelAttemptFailureKind::InvalidRequest
        );
    }

    assert!(
        secret_store
            .resolutions
            .lock()
            .expect("lock SecretStore calls")
            .is_empty()
    );
    assert_eq!(probe.calls.load(Ordering::Relaxed), 0);
    assert_eq!(admission.reserves.load(Ordering::Relaxed), 0);
    fs::remove_dir_all(root).expect("remove Gateway fixture directory");
}

#[test]
fn strict_json_schema_reaches_a_model_that_declares_strict_support() {
    let root = temporary_directory("structured-output-supported");
    let mut storage = SqliteStorage::open(&root).expect("open Gateway storage");
    register_provider_with_structured_output(
        &mut storage,
        91,
        0,
        "openai",
        "gpt-fixture",
        91,
        StructuredOutputSupport::JsonSchemaStrict,
    );
    create_credential(&mut storage, 92, 91, "openai");
    configure_session(&mut storage, 93, 91, "openai", "gpt-fixture", 91);

    let identity = FakeIdentity {
        repository_scope: repository_scope(),
        deny: AtomicBool::new(false),
    };
    let secret = b"openai-secret-fixture".to_vec();
    let secret_store = FakeSecretStore {
        secrets: BTreeMap::from([("openai".to_owned(), secret.clone())]),
        resolutions: Mutex::new(Vec::new()),
    };
    let settlement = SettlementProbe::default();
    let mut admission = AdmissionProbe::default();
    let probe = Arc::new(AdapterProbe::default());
    let message = open_message(
        91,
        91,
        191,
        91,
        br#"{"request":{"text":{"format":{"type":"json_schema","strict":true,"name":"change_batch","schema":{"type":"object"}}}}}"#,
    );
    {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secret_store,
            &identity,
            &settlement,
            &mut admission,
        );
        gateway
            .register_adapter(Box::new(FakeAdapter {
                provider_id: "openai".to_owned(),
                expected_secret: secret,
                receipt_id: None,
                probe: Arc::clone(&probe),
            }))
            .expect("register OpenAI fixture adapter");
        gateway
            .open(
                &message,
                &route("openai", "gpt-fixture", 91),
                &adapter_request_id(&message),
            )
            .expect("strict-capable model opens Provider");
    }
    assert_eq!(probe.calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        secret_store
            .resolutions
            .lock()
            .expect("lock SecretStore calls")
            .len(),
        1
    );
    fs::remove_dir_all(root).expect("remove Gateway fixture directory");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the table pins every structured-output preflight shape and its side-effect boundary"
)]
fn structured_output_preflight_accepts_only_valid_non_strict_shapes_before_provider_io() {
    let root = temporary_directory("structured-output-shape-matrix");
    let mut storage = SqliteStorage::open(&root).expect("open Gateway storage");
    register_provider_with_structured_output(
        &mut storage,
        101,
        0,
        "anthropic",
        "claude-fixture",
        101,
        StructuredOutputSupport::Unsupported,
    );
    create_credential(&mut storage, 102, 101, "anthropic");
    configure_session(&mut storage, 103, 101, "anthropic", "claude-fixture", 101);

    let identity = FakeIdentity {
        repository_scope: repository_scope(),
        deny: AtomicBool::new(false),
    };
    let secret = b"anthropic-shape-matrix-secret".to_vec();
    let secret_store = FakeSecretStore {
        secrets: BTreeMap::from([("anthropic".to_owned(), secret.clone())]),
        resolutions: Mutex::new(Vec::new()),
    };
    let settlement = SettlementProbe::default();
    let mut admission = AdmissionProbe::default();
    let probe = Arc::new(AdapterProbe::default());
    let cases = [
        (
            "request_missing",
            serde_json::json!({"prompt":"plain"}),
            None,
        ),
        ("text_missing", serde_json::json!({"request":{}}), None),
        (
            "text_null",
            serde_json::json!({"request":{"text":null}}),
            None,
        ),
        (
            "format_missing",
            serde_json::json!({"request":{"text":{"verbosity":"low"}}}),
            None,
        ),
        (
            "format_null",
            serde_json::json!({"request":{"text":{"format":null}}}),
            None,
        ),
        (
            "plain_text",
            serde_json::json!({"request":{"text":{"format":{"type":"text"}}}}),
            None,
        ),
        (
            "json_schema_non_strict",
            serde_json::json!({"request":{"text":{"format":{
                "type":"json_schema", "strict":false, "name":"result", "schema":{}
            }}}}),
            None,
        ),
        (
            "request_not_object",
            serde_json::json!({"request":[]}),
            Some(ProviderGatewayErrorKind::InvalidRequest),
        ),
        (
            "text_not_object",
            serde_json::json!({"request":{"text":"plain"}}),
            Some(ProviderGatewayErrorKind::InvalidRequest),
        ),
        (
            "format_not_object",
            serde_json::json!({"request":{"text":{"format":"json_schema"}}}),
            Some(ProviderGatewayErrorKind::InvalidRequest),
        ),
        (
            "unknown_format_type",
            serde_json::json!({"request":{"text":{"format":{"type":"future"}}}}),
            Some(ProviderGatewayErrorKind::InvalidRequest),
        ),
        (
            "strict_wrong_type",
            serde_json::json!({"request":{"text":{"format":{
                "type":"json_schema", "strict":"true", "name":"result", "schema":{}
            }}}}),
            Some(ProviderGatewayErrorKind::InvalidRequest),
        ),
        (
            "schema_wrong_type",
            serde_json::json!({"request":{"text":{"format":{
                "type":"json_schema", "strict":true, "name":"result", "schema":[]
            }}}}),
            Some(ProviderGatewayErrorKind::InvalidRequest),
        ),
        (
            "empty_name",
            serde_json::json!({"request":{"text":{"format":{
                "type":"json_schema", "strict":true, "name":"", "schema":{}
            }}}}),
            Some(ProviderGatewayErrorKind::InvalidRequest),
        ),
        (
            "description_wrong_type",
            serde_json::json!({"request":{"text":{"format":{
                "type":"json_schema", "strict":true, "name":"result", "schema":{},
                "description":false
            }}}}),
            Some(ProviderGatewayErrorKind::InvalidRequest),
        ),
        (
            "required_field_missing",
            serde_json::json!({"request":{"text":{"format":{
                "type":"json_schema", "strict":true, "schema":{}
            }}}}),
            Some(ProviderGatewayErrorKind::InvalidRequest),
        ),
        (
            "unknown_field",
            serde_json::json!({"request":{"text":{"format":{
                "type":"json_schema", "strict":true, "name":"result", "schema":{},
                "future":true
            }}}}),
            Some(ProviderGatewayErrorKind::InvalidRequest),
        ),
        (
            "strict_supported_shape",
            serde_json::json!({"request":{"text":{"format":{
                "type":"json_schema", "strict":true, "name":"result", "schema":{}
            }}}}),
            Some(ProviderGatewayErrorKind::StructuredOutputUnsupported),
        ),
    ];

    {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secret_store,
            &identity,
            &settlement,
            &mut admission,
        );
        gateway
            .register_adapter(Box::new(FakeAdapter {
                provider_id: "anthropic".to_owned(),
                expected_secret: secret,
                receipt_id: None,
                probe: Arc::clone(&probe),
            }))
            .expect("register Anthropic fixture adapter");

        let mut successful = 0_u64;
        for (index, (name, value, expected_error)) in cases.into_iter().enumerate() {
            let payload = serde_json::to_vec(&value).expect("encode shape fixture");
            let seed = 201 + u64::try_from(index).expect("fixture index fits u64");
            let message = open_message(seed, seed, seed + 100, 101, &payload);
            let before_secret = secret_store
                .resolutions
                .lock()
                .expect("lock SecretStore calls")
                .len();
            let before_adapter = probe.calls.load(Ordering::Relaxed);
            let result = gateway.open(
                &message,
                &route("anthropic", "claude-fixture", 101),
                &adapter_request_id(&message),
            );
            if let Some(expected) = expected_error {
                assert_eq!(
                    result.expect_err(name).kind(),
                    expected,
                    "unexpected preflight result for {name}"
                );
                assert_eq!(
                    secret_store
                        .resolutions
                        .lock()
                        .expect("lock SecretStore calls")
                        .len(),
                    before_secret,
                    "{name} read a secret"
                );
                assert_eq!(
                    probe.calls.load(Ordering::Relaxed),
                    before_adapter,
                    "{name} called the Provider"
                );
            } else {
                result.unwrap_or_else(|error| panic!("{name} should pass preflight: {error}"));
                successful += 1;
            }
        }

        let malformed = open_message(
            301,
            301,
            401,
            101,
            br#"{"request":{"text":{"format":{"type":"json_schema""#,
        );
        let before_secret = secret_store
            .resolutions
            .lock()
            .expect("lock SecretStore calls")
            .len();
        let before_adapter = probe.calls.load(Ordering::Relaxed);
        assert_eq!(
            gateway
                .open(
                    &malformed,
                    &route("anthropic", "claude-fixture", 101),
                    &adapter_request_id(&malformed),
                )
                .expect_err("malformed JSON must fail preflight")
                .kind(),
            ProviderGatewayErrorKind::InvalidRequest
        );
        assert_eq!(
            secret_store
                .resolutions
                .lock()
                .expect("lock SecretStore calls")
                .len(),
            before_secret,
            "malformed JSON read a secret"
        );
        assert_eq!(
            probe.calls.load(Ordering::Relaxed),
            before_adapter,
            "malformed JSON called the Provider"
        );
        assert_eq!(successful, 7);
    }
    assert_eq!(probe.calls.load(Ordering::Relaxed), 7);
    assert_eq!(admission.reserves.load(Ordering::Relaxed), 7);
    assert_eq!(
        secret_store
            .resolutions
            .lock()
            .expect("lock SecretStore calls")
            .len(),
        7
    );
    fs::remove_dir_all(root).expect("remove Gateway fixture directory");
}

#[test]
fn invalid_payload_digest_and_unregistered_adapter_fail_before_secret_resolution() {
    let root = temporary_directory("early-rejection");
    let mut storage = SqliteStorage::open(&root).expect("open Gateway storage");
    register_provider(&mut storage, 11, 0, "provider-a", "model-a", 11);
    create_credential(&mut storage, 12, 11, "provider-a");
    configure_session(&mut storage, 13, 11, "provider-a", "model-a", 11);

    let identity = FakeIdentity {
        repository_scope: repository_scope(),
        deny: AtomicBool::new(false),
    };
    let secret_store = FakeSecretStore {
        secrets: BTreeMap::from([(
            "provider-a".to_owned(),
            b"provider-a-secret-fixture".to_vec(),
        )]),
        resolutions: Mutex::new(Vec::new()),
    };
    let settlement = SettlementProbe::default();
    let mut admission = AdmissionProbe::default();
    let mut gateway = ProviderGateway::new(
        &mut storage,
        &secret_store,
        &identity,
        &settlement,
        &mut admission,
    );
    let mut message = open_message(11, 11, 111, 11, br#"{"prompt":"alpha"}"#);
    let adapter_missing = gateway
        .open(
            &message,
            &route("provider-a", "model-a", 11),
            &adapter_request_id(&message),
        )
        .expect_err("unregistered adapter is explicit");
    assert_eq!(
        adapter_missing.kind(),
        ProviderGatewayErrorKind::AdapterNotRegistered
    );
    assert!(
        secret_store
            .resolutions
            .lock()
            .expect("lock calls")
            .is_empty()
    );

    gateway
        .register_adapter(Box::new(FakeAdapter {
            provider_id: "provider-a".to_owned(),
            expected_secret: b"provider-a-secret-fixture".to_vec(),
            receipt_id: Some("provider-a-secret-fixture".to_owned()),
            probe: Arc::new(AdapterProbe::default()),
        }))
        .expect("register Provider adapter");
    message.request.payload_digest = Sha256Digest("sha256:bad".to_owned());
    let invalid_digest = gateway
        .open(
            &message,
            &route("provider-a", "model-a", 11),
            &adapter_request_id(&message),
        )
        .expect_err("invalid payload digest is rejected");
    assert_eq!(
        invalid_digest.kind(),
        ProviderGatewayErrorKind::InvalidRequest
    );
    assert!(
        secret_store
            .resolutions
            .lock()
            .expect("lock calls")
            .is_empty()
    );
    message.request = encoded_payload(br#"{"prompt":"alpha"}"#);
    let leaking_receipt = gateway
        .open(
            &message,
            &route("provider-a", "model-a", 11),
            &adapter_request_id(&message),
        )
        .expect_err("adapter receipt cannot copy Credential bytes");
    assert_eq!(
        leaking_receipt.kind(),
        ProviderGatewayErrorKind::CredentialLeak
    );
    drop(gateway);

    fs::remove_dir_all(root).expect("remove Gateway fixture directory");
}

struct FailingAdapter {
    provider_id: String,
    expected_secret: Vec<u8>,
    error: ProviderAdapterError,
    calls: Arc<AtomicU64>,
}

impl ProviderAdapterPort for FailingAdapter {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn open(
        &self,
        _invocation: &ProviderAdapterInvocation<'_>,
        credential: &ResolvedSecret,
    ) -> Result<ProviderAdapterOpenReceipt, ProviderAdapterError> {
        assert_eq!(credential.expose(), self.expected_secret);
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(self.error.clone())
    }

    fn control(
        &self,
        _model_exchange_id: &ModelExchangeId,
        _adapter_request_id: &str,
        _action: ProviderStreamControlAction,
    ) -> Result<(), ProviderAdapterError> {
        Ok(())
    }
}

fn inject_provider_quota_rollback_failure(root: &Path) {
    Connection::open(root.join("control-plane.sqlite3"))
        .expect("open quota rollback failure injector")
        .execute_batch(
            "CREATE TRIGGER fail_provider_quota_rollback
             BEFORE UPDATE ON enterprise_quota_reservations
             WHEN OLD.state = 'active' AND NEW.state = 'released'
             BEGIN SELECT RAISE(FAIL, 'injected Provider quota rollback failure'); END;",
        )
        .expect("install quota rollback failure");
}

fn assert_enterprise_quota_replay(
    quota: &mut DurableEnterpriseQuotaAdmission,
    context: &FixedRetryContext,
    fail_rollback: bool,
) {
    let replay = quota
        .reserve(context.0.enterprise_quota_request())
        .expect("replay enterprise quota reservation");
    if fail_rollback {
        let EnterpriseQuotaAdmission::Admitted(permit) = replay else {
            panic!("failed rollback must leave the enterprise reservation active");
        };
        assert!(permit.receipt().idempotent_replay);
        assert_eq!(
            permit.receipt().record.state,
            EnterpriseQuotaReservationState::Active
        );
        assert!(permit.receipt().record.terminal.is_none());
    } else {
        let EnterpriseQuotaAdmission::TerminalReplay(receipt) = replay else {
            panic!("Provider failure must leave a terminal enterprise quota release");
        };
        assert!(receipt.idempotent_replay);
        assert_eq!(
            receipt.record.state,
            EnterpriseQuotaReservationState::Released
        );
        assert!(matches!(
            receipt.record.terminal,
            Some(EnterpriseQuotaTerminal::Released {
                reason: EnterpriseQuotaReleaseReason::OperationalAdmissionDenied,
                ..
            })
        ));
    }
}

fn assert_provider_error_survives_enterprise_quota_rollback(
    label: &str,
    seed: u64,
    adapter_error: ProviderAdapterError,
    expected: ProviderGatewayErrorKind,
    fail_rollback: bool,
) {
    let root = temporary_directory(label);
    let mut storage = SqliteStorage::open(&root).expect("open quota rollback Gateway storage");
    register_provider(&mut storage, seed, 0, "provider-a", "model-a", seed);
    create_credential(&mut storage, seed + 1, seed, "provider-a");
    configure_session(&mut storage, seed + 2, seed, "provider-a", "model-a", seed);
    let message = open_message(
        seed,
        seed,
        seed + 100,
        seed,
        br#"{"prompt":"quota rollback"}"#,
    );
    let context = FixedRetryContext(retry_context(&mut storage, &message));
    let identity = FakeIdentity {
        repository_scope: repository_scope(),
        deny: AtomicBool::new(false),
    };
    let fixture_bytes = b"provider-a-secret-fixture".to_vec();
    let secret_store = FakeSecretStore {
        secrets: BTreeMap::from([("provider-a".to_owned(), fixture_bytes.clone())]),
        resolutions: Mutex::new(Vec::new()),
    };
    let settlement = SettlementProbe::default();
    let mut admission = AdmissionProbe::default();
    let calls = Arc::new(AtomicU64::new(0));
    let mut quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(&root).expect("open enterprise quota storage"),
    );
    if fail_rollback {
        let EnterpriseQuotaAdmission::Admitted(permit) = quota
            .reserve(context.0.enterprise_quota_request())
            .expect("seed active enterprise quota reservation")
        else {
            panic!("first enterprise quota reservation must be admitted");
        };
        assert!(!permit.receipt().idempotent_replay);
        inject_provider_quota_rollback_failure(&root);
    }
    {
        let mut gateway = ProviderGateway::new(
            &mut storage,
            &secret_store,
            &identity,
            &settlement,
            &mut admission,
        );
        gateway
            .register_adapter(Box::new(FailingAdapter {
                provider_id: "provider-a".to_owned(),
                expected_secret: fixture_bytes,
                error: adapter_error,
                calls: Arc::clone(&calls),
            }))
            .expect("register failing Provider adapter");
        let reservation = gateway
            .reserve_before_open(&message)
            .expect("reserve Provider capacity before enterprise quota");
        let error = gateway
            .open_after_reservation_with_enterprise_quota(
                &message,
                &reservation,
                &adapter_request_id(&message),
                &context,
                &mut quota,
            )
            .expect_err("Provider failure must survive enterprise quota rollback");
        assert_eq!(error.kind(), expected);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        if !fail_rollback {
            let replay = gateway
                .open_after_reservation_with_enterprise_quota(
                    &message,
                    &reservation,
                    &adapter_request_id(&message),
                    &context,
                    &mut quota,
                )
                .expect_err("terminal quota replay must not call the Provider again");
            assert_eq!(replay.kind(), ProviderGatewayErrorKind::AdmissionDenied);
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }
    }
    assert_eq!(admission.terminals.load(Ordering::Relaxed), 1);
    assert!(
        admission
            .reserved
            .lock()
            .expect("lock Provider admission reservations")
            .is_empty()
    );
    quota.close().expect("close enterprise quota storage");
    let mut restarted_quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(&root).expect("restart enterprise quota storage"),
    );
    assert_enterprise_quota_replay(&mut restarted_quota, &context, fail_rollback);
    restarted_quota
        .close()
        .expect("close restarted enterprise quota storage");
    drop(storage);
    fs::remove_dir_all(root).expect("remove quota rollback fixture");
}

#[test]
fn enterprise_quota_rollback_preserves_every_provider_adapter_failure_category() {
    for (label, seed, error, expected) in [
        (
            "quota-adapter-rejected",
            301,
            ProviderAdapterError::rejected(),
            ProviderGatewayErrorKind::AdapterRejected,
        ),
        (
            "quota-adapter-rate-limited",
            311,
            ProviderAdapterError::rate_limited(),
            ProviderGatewayErrorKind::AdapterRateLimited,
        ),
        (
            "quota-adapter-unavailable",
            321,
            ProviderAdapterError::unavailable(),
            ProviderGatewayErrorKind::AdapterUnavailable,
        ),
        (
            "quota-adapter-protocol",
            331,
            ProviderAdapterError::protocol(),
            ProviderGatewayErrorKind::AdapterProtocol,
        ),
    ] {
        assert_provider_error_survives_enterprise_quota_rollback(
            label, seed, error, expected, false,
        );
    }
}

#[test]
fn enterprise_quota_rollback_failure_does_not_expose_the_provider_error() {
    assert_provider_error_survives_enterprise_quota_rollback(
        "quota-rollback-failure",
        341,
        ProviderAdapterError::rejected(),
        ProviderGatewayErrorKind::AdmissionUnavailable,
        true,
    );
}
